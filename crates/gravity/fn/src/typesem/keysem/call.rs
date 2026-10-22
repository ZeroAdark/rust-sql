//! KeySem's run-time calls (decision 24, K5): a key's support function called by the executor, one bound-call table per
//! thread for every caller (the sort's comparator, the pull-form merge's held compare, the streamed group's user `=`).
//! A call is bound on first use by `(resolution id, collation, use)` (`crate::resolved`: resolved at lowering, so a pool
//! worker binds a copy with no catalog access) and kept for the session's life (`sweep_at_session_end`) while what its
//! bind and first call read stands (C3: `Bound::held`); its result context is reset every `WINDOW` calls (a support function's detoasted copies of its arguments are its context's).
//! The table is not borrowed while a call runs (D3375 N7): a SQL / PL function's own statements are served like any
//! nested statement, and one that sorts under a user comparator reaches this table again and binds its own copy.
//! D3402: a SQL comparator whose body is one built-in call over its arguments binds that built-in (`alias`).
use core::cmp::Ordering;

use crate::{Datum, FmgrCall, PgError};

mod alias;
mod raw;
use alias::Alias;
pub use alias::{set_alias_verdict, AliasVerdict};
use raw::{Running, Slot};

/// What the bound function is to its caller.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Use {
    /// A btree support function 1 (negative, zero or positive), never counted under `track_functions` (D2835:
    /// PostgreSQL's sort calls it through `comparison_shim`).
    Order,
    /// An `=`'s function, counted as PostgreSQL's ExecBuildGroupingEqual counts it (D2719).
    Equal,
}

struct Bound {
    proc: u32,
    coll: u32,
    use_: Use,
    calls: u32,
    /// D3375 N7: a call of this binding is running on this thread (its function's body may be running statements). A
    /// nested call binds its own (PostgreSQL's nested sort has its own `FmgrInfo`), and nothing else reads the binding
    /// but this flag and `stale` (`Slot`).
    busy: bool,
    /// D3375 N7: the session ended (`sweep`) while a call of this binding ran: never found again, dropped when that call
    /// ends (`Running`).
    stale: bool,
    /// C3: what the bind and its first call read (`first_call`: the function's pg_proc row, and what its `fn_extra` state
    /// was built from: a record or array comparator's type cache entries, a collation's); a call finds the binding only
    /// while those reads stand (`find`). `None` until that first call ends.
    held: Option<gravity_deps::Held>,
    f: FmgrCall,
    /// `order_seen`'s values (an `Order` binding under no collation): equal copies whose compare ran clean.
    seen: std::collections::HashSet<Box<[u8]>>, // alloc: Exec
    /// D3402: `f` is the built-in a SQL comparator's body calls, over the arguments this permutes (`alias`); found only
    /// while `Alias::current`.
    alias: Option<Alias>,
}

std::thread_local! {
    static BOUND: core::cell::RefCell<Vec<Slot>> = const { core::cell::RefCell::new(Vec::new()) }; // alloc: Cold
    /// D2372: this thread's session teardown empties `BOUND` (registered with its first binding).
    static SWEPT: core::cell::Cell<bool> = const { core::cell::Cell::new(false) };
}
const WINDOW: u32 = 1024;

/// D2372: a bound function's own state lives in the session's memory (a record comparator's `fn_extra`), which the
/// session teardown frees before the thread ends; dropped by the thread's destructor after that, a free into a context
/// already gone faulted the server at session end. The teardown (`crate::at_session_end`) empties `BOUND` instead,
/// once per session; a thread with no session teardown (a unit test) never runs it.
fn sweep_at_session_end() {
    if SWEPT.with(|s| s.replace(true)) {
        return;
    }
    crate::at_session_end(sweep);
}

fn sweep() {
    let bound = BOUND.try_with(|h| {
        h.try_borrow_mut()
            .map(|mut v| {
                let (running, idle): (Vec<Slot>, Vec<Slot>) = core::mem::take(&mut *v).into_iter().partition(Slot::busy); // alloc: Cold
                running.iter().for_each(Slot::mark_stale);
                *v = running;
                idle
            })
            .unwrap_or_default()
    });
    drop(bound);
    let _ = SWEPT.try_with(|s| s.set(false));
}

/// `proc(a, b)` under `coll` through its binding for `use_` (bound here on first use), its result read by `read` while the
/// binding is held, so what leaves is a word-sized answer (an `Ordering`, a `bool`). The table's borrow ends before the
/// call (D3375 N7: the function's own statements are served and may reach the table; `Running` marks the binding).
/// Inlined into its callers (`order`, `equal`, `order_pair` and theirs), as each caller's own table was before K5: a
/// sort calls it once per compared pair. K5's quiet pass and a
/// perf annotate on box 87 read both costs: out-of-line frames, and the 24-byte call result copied out through the
/// thread-local's return at odd offsets (a store-forwarding stall per call), together 1.14-1.17x on a LANGUAGE internal
/// comparator and an enum key.
#[inline]
fn call<T>(proc: u32, coll: u32, use_: Use, a: u64, b: u64, read: impl FnOnce(crate::NullableDatum) -> Result<T, Box<PgError>>) -> Result<T, Box<PgError>> {
    let Some(mut run) = BOUND.with(|h| find(&mut h.borrow_mut(), proc, coll, use_)) else {
        return first_call(proc, coll, use_, a, b, read);
    };
    // `run` keeps the binding live and busy (no other caller references it) until it drops, after `read`
    let out = match call_run(&mut run, a, b) {
        Ok(r) => read(r),
        Err(err) => Err(err),
    };
    drop(run);
    out
}

/// C3 (KeySem's run-time cache): a new binding's first call, the bind and the call recorded (gravity_deps `Held::fill`)
/// and held by the binding: a function's `fn_extra` state is built at its first call (array_eq's and record_eq's type
/// cache entries, varstr_cmp's collation), so a change to what those read (an element's opclass, a support function
/// replaced, a type edited) drops the binding at the next call that finds it, which binds afresh with an empty
/// `fn_extra`, as PostgreSQL's executor binds at every start. A pool worker's bind reads no catalog (`crate::resolved`):
/// its bindings hold nothing and stand.
#[cold]
#[inline(never)]
fn first_call<T>(proc: u32, coll: u32, use_: Use, a: u64, b: u64, read: impl FnOnce(crate::NullableDatum) -> Result<T, Box<PgError>>) -> Result<T, Box<PgError>> {
    let (first, held) = gravity_deps::Held::fill(|| -> Result<_, Box<PgError>> {
        let mut run = bind(proc, coll, use_)?;
        let r = call_run(&mut run, a, b);
        Ok((run, r))
    });
    let (mut run, r) = first?;
    run.hold(held);
    let out = match r {
        Ok(r) => read(r),
        Err(err) => Err(err),
    };
    drop(run);
    out
}

/// The running binding's call over `a`, `b`: its built-in's over the permuted pair when it is an alias (D3402).
#[inline]
fn call_run(run: &mut Running, a: u64, b: u64) -> Result<crate::NullableDatum, Box<PgError>> {
    let (x, y) = match run.perm() {
        Some([0, _]) | None => (a, b),
        Some(_) => (b, a),
    };
    run.f().call_site(&[Datum::from_u64(x), Datum::from_u64(y)], &[false, false])
}

/// An idle binding of `(proc, coll, use_)` whose recorded reads stand (`Bound::held`; and, for an alias, under the current
/// user and search path), its call counted and started. One whose reads moved leaves the table here (the caller binds
/// afresh); a running one is never touched (D3375 N7: its call ends first, then the next find drops it).
#[inline]
fn find(h: &mut Vec<Slot>, proc: u32, coll: u32, use_: Use) -> Option<Running> {
    let i = h.iter().position(|s| s.idle().is_some_and(|e| e.proc == proc && e.coll == coll && e.use_ == use_ && !e.stale && !e.alias.is_some_and(|a| !a.current())))?;
    if !h[i].idle().is_some_and(|e| e.held.as_ref().is_none_or(gravity_deps::Held::stands)) {
        drop(h.swap_remove(i));
        return None;
    }
    let s = &mut h[i];
    let e = s.idle_mut().expect("gravity: the binding found idle");
    e.calls += 1;
    if e.calls % WINDOW == 0 {
        e.f.reset_window();
    }
    Some(s.run())
}

/// A new binding of `(proc, coll, use_)`, its first call started. A SQL comparator under no collation is read for its
/// built-in alias first (D3402), outside the table's borrow (the read runs catalog code); an alias binding no longer
/// current (the user or the search path moved) leaves the table here.
#[cold]
fn bind(proc: u32, coll: u32, use_: Use) -> Result<Running, Box<PgError>> {
    let alias = if use_ == Use::Order && coll == 0 { alias::read(proc) } else { None };
    BOUND.with(|h| {
        let mut h = h.borrow_mut();
        h.retain(|s| !s.idle().is_some_and(|e| e.alias.is_some_and(|a| !a.current())));
        let Some(f) = FmgrCall::bind_resolved(alias.map_or(proc, |(b, _)| b), 2, coll) else {
            return Err(Box::new(PgError::error(match use_ { // alloc: Cold
                Use::Order => "gravity: a comparator no lowering resolved",
                Use::Equal => "gravity: an equality no lowering resolved",
            })));
        };
        let f = if use_ == Use::Order { f.untracked() } else { f };
        let alias = alias.map(|(_, a)| a);
        h.push(Slot::new(Bound { proc, coll, use_, calls: 1, busy: false, stale: false, held: None, f, seen: std::collections::HashSet::new(), alias })); // alloc: Cold
        sweep_at_session_end();
        let i = h.len() - 1;
        Ok(h[i].run())
    })
}

/// A support function's call runs on this backend (a binding is busy): a statement running now is one its body runs,
/// once per compared pair. Off the hot path only (gravity_exec's `Backend::reporting` reads it while the report is on,
/// never `call`).
pub fn calling() -> bool {
    BOUND.try_with(|h| h.try_borrow().is_ok_and(|v| v.iter().any(|s| s.busy()))).unwrap_or(false)
}

/// Btree support function `proc` (a resolution id) over `a`, `b` under `coll`. `Err`: the bind or the call raised, or
/// the function returned NULL (PostgreSQL's sort raises there too).
#[inline]
pub fn order(proc: u32, coll: u32, a: u64, b: u64) -> Result<Ordering, Box<PgError>> {
    call(proc, coll, Use::Order, a, b, |r| match r {
        r if !r.isnull => Ok(r.value.as_i32().cmp(&0)),
        _ => Err(Box::new(PgError::error("gravity: a btree comparator returned NULL"))), // alloc: Cold
    })
}

/// `=` function `proc` (a resolution id) over `a`, `b` under `coll`: true when it returns true, false when it returns
/// false or NULL (EEOP_QUAL reads a NULL as not equal). `Err`: the bind or the call raised.
#[inline]
pub fn equal(proc: u32, coll: u32, a: u64, b: u64) -> Result<bool, Box<PgError>> {
    call(proc, coll, Use::Equal, a, b, |r| Ok(!r.isnull && r.value.as_bool()))
}

/// How a sort key's two stored copies compare as bytes, read by the caller before any call.
#[derive(Clone, Copy, Debug)]
pub enum Copies<'a> {
    Differ,
    /// Equal words or equal fixed images.
    Same,
    /// Equal var copies, with their payload.
    SameVar(&'a [u8]),
}

/// A sort key's pair `da`, `db` ordered by comparator `proc` under `coll`, its copies' byte comparison `copies` first:
/// equal copies are equal values (a btree order is reflexive) with no call, except (D2043) equal var copies under no
/// collation, which a record or array of a collatable column without one makes PostgreSQL raise 42P22 at its first
/// compare (`record_cmp` -> `varstr_cmp`), so the first pair of each value calls (`order_seen`) unless `eqtie`
/// (`KFLAG_EQTIE`: the key's type has no collatable part) says no such raise can arise; `user` (`KFLAG_USER`): every pair
/// calls the user's comparator, equal copies too, as PostgreSQL's sort does (it may raise on them; its caller may pass
/// `Differ` without comparing).
#[inline]
pub fn order_pair(proc: u32, coll: u32, eqtie: bool, user: bool, copies: Copies<'_>, da: u64, db: u64) -> Result<Ordering, Box<PgError>> {
    if user {
        return order(proc, coll, da, db);
    }
    match copies {
        Copies::Differ => order(proc, coll, da, db),
        Copies::Same => Ok(Ordering::Equal),
        Copies::SameVar(_) if coll != 0 || eqtie => Ok(Ordering::Equal),
        Copies::SameVar(payload) => order_seen(proc, payload, da, db),
    }
}

/// The values `order_seen` keeps per comparator binding under no collation, each of at most `SEEN_BYTES` (past either, an
/// equal copy calls as before): at most a megabyte, its lookups cheap beside a call.
const SEEN_VALUES: usize = 4096;
const SEEN_BYTES: usize = 256;

/// D2043's equal copies `da` / `db` of `payload` under no collation (aggtrans 0387): the first pair of a value calls the
/// comparator; once that call ran clean the value's equal copies tie without one (the same bytes compare the same way: a
/// btree order is reflexive and the call deterministic), so a key of few values (a composite) calls about as often as
/// PostgreSQL's sort. A call that failed is never remembered (its error is the sort's).
fn order_seen(proc: u32, payload: &[u8], da: u64, db: u64) -> Result<Ordering, Box<PgError>> {
    if payload.len() > SEEN_BYTES {
        return order(proc, 0, da, db);
    }
    let known = BOUND.with(|h| h.borrow().iter().filter_map(Slot::idle).any(|e| e.proc == proc && e.coll == 0 && e.use_ == Use::Order && e.seen.contains(payload)));
    if known {
        return Ok(Ordering::Equal);
    }
    let o = order(proc, 0, da, db)?;
    BOUND.with(|h| {
        if let Some(e) = h.borrow_mut().iter_mut().filter_map(Slot::idle_mut).find(|e| e.proc == proc && e.coll == 0 && e.use_ == Use::Order).filter(|e| e.seen.len() < SEEN_VALUES) {
            e.seen.insert(payload.into()); // alloc: Exec
        }
    });
    Ok(o)
}

/// Empties every binding's `seen` values (`order_seen`'s): a sort's, a put's or a merge's comparator starts, so they never
/// outlive the statement whose types they saw.
pub fn forget_seen() {
    BOUND.with(|h| h.borrow_mut().iter_mut().filter_map(Slot::idle_mut).for_each(|e| e.seen.clear()));
}

#[cfg(test)]
mod tests {
    use super::*;

    /// btint4cmp (351), the fixture comparator of resolved.rs's tests: no catalog. OID-17 (overfit, keep): test-only.
    fn slot(id: u32, held: Option<gravity_deps::Held>) -> Slot {
        let f = FmgrCall::bind_resolved(id, 2, 0).expect("a resolved builtin binds");
        Slot::new(Bound { proc: id, coll: 0, use_: Use::Order, calls: 0, busy: false, stale: false, held, f, seen: std::collections::HashSet::new(), alias: None })
    }

    /// D3375 N7 + C3: a call reaches its binding through `Running` while the table changes under it (its function's body
    /// runs statements that bind, sweep or change the catalog): `idle` never hands out the busy binding, a find drops an
    /// idle binding whose recorded reads moved (`Bound::held`) and never the busy one, the call goes on through its
    /// pointer, and the binding leaves the table at the first find after its call ended; a fresh call binds anew and holds
    /// what its bind and first call read.
    #[test]
    fn a_binding_whose_reads_moved_leaves_at_a_find_and_a_running_one_after_its_call() {
        let id = crate::resolved::resolve(351).unwrap().unwrap();
        let r: u32 = 0xFFFF_F3E1;
        let held = || gravity_deps::Held::fill(|| gravity_deps::note_rel(r)).1;
        BOUND.with(|h| {
            let mut h = h.borrow_mut();
            h.clear();
            h.push(slot(id, Some(held())));
            h.push(slot(id, Some(held())));
        });
        let mut run = BOUND.with(|h| h.borrow_mut()[0].run());
        BOUND.with(|h| {
            let h = h.borrow();
            assert!(h[0].idle().is_none() && h[1].idle().is_some(), "only the idle binding is handed out");
        });
        gravity_deps::invalidate_rel_for_test(r);
        BOUND.with(|h| {
            let mut h = h.borrow_mut();
            assert!(find(&mut h, id, 0, Use::Order).is_none(), "the idle binding's read moved");
            assert_eq!(h.len(), 1, "the find dropped the idle binding and kept the running one");
            assert!(h[0].busy() && h[0].runs(&run));
        });
        // `run` keeps the binding live and busy; this is its call
        let r = run.f().call_site(&[Datum::from_u64(1), Datum::from_u64(2)], &[false, false]).expect("btint4cmp");
        assert!(r.value.as_i32() < 0);
        drop(run);
        BOUND.with(|h| {
            let mut h = h.borrow_mut();
            assert!(h.len() == 1 && h[0].idle().is_some(), "its call ended: idle, still in the table");
            assert!(find(&mut h, id, 0, Use::Order).is_none() && h.is_empty(), "the next find dropped it");
        });
        // a fresh call binds anew, its first call recorded: one idle binding that stands
        assert_eq!(order(id, 0, 2, 1).expect("order"), Ordering::Greater);
        BOUND.with(|h| {
            let h = h.borrow();
            assert!(h.len() == 1 && h[0].idle().is_some_and(|e| e.held.as_ref().is_some_and(gravity_deps::Held::stands)));
        });
        assert_eq!(order(id, 0, 1, 2).expect("order"), Ordering::Less);
        BOUND.with(|h| assert_eq!(h.borrow().len(), 1, "the second call found the first's binding"));
    }

    /// D3402: an alias binding calls its built-in over the permuted pair (btint4cmp bound as `SELECT btint4cmp($2, $1)`
    /// orders 1 after 2), the identity permutation as the built-in itself.
    #[test]
    fn an_alias_binding_calls_its_builtin_over_the_permuted_pair() {
        let id = crate::resolved::resolve(351).unwrap().unwrap();
        for (perm, want) in [([1u8, 0u8], Ordering::Greater), ([0, 1], Ordering::Less)] {
            let f = FmgrCall::bind_resolved(id, 2, 0).expect("a resolved builtin binds");
            let alias = Some(Alias::for_test(perm));
            let mut s = Slot::new(Bound { proc: id, coll: 0, use_: Use::Order, calls: 0, busy: false, stale: false, held: None, f, seen: std::collections::HashSet::new(), alias });
            let mut run = s.run();
            assert_eq!(run.perm(), Some(perm));
            let r = call_run(&mut run, 1, 2).expect("btint4cmp");
            assert_eq!(r.value.as_i32().cmp(&0), want, "{perm:?}");
            drop(run);
            drop(s);
        }
    }

    /// D3402: `alias::one_call` reads a body that is one call over `$1` and `$2` (each at most relabelled) under no input
    /// collation and returning the comparator's type, and nothing else.
    #[test]
    fn one_call_reads_a_permuted_builtin_call_only() {
        use types_nodes::primnodes::{CoercionForm, FuncExpr, OpExpr, Param, ParamKind, RelabelType};
        use types_nodes::{Node, NodeList};
        fn param(m: mcx::Mcx<'_>, id: i32) -> Node<'_> {
            Node::mk(m, Param { paramkind: ParamKind::PARAM_EXTERN, paramid: id, paramtype: 20, paramtypmod: -1, location: -1, ..Default::default() }).unwrap()
        }
        fn relabel<'m>(m: mcx::Mcx<'m>, n: Node<'m>) -> Node<'m> {
            Node::mk(m, RelabelType { arg: n, resulttype: 20, resulttypmod: -1, resultcollid: 0, relabelformat: CoercionForm::COERCE_IMPLICIT_CAST, location: -1 }).unwrap()
        }
        fn call<'m>(m: mcx::Mcx<'m>, args: &[Node<'m>], ret: u32, coll: u32) -> Node<'m> {
            Node::mk(m, FuncExpr { funcid: 2189, funcresulttype: ret, inputcollid: coll, args: NodeList::from_slice(m, args).unwrap(), ..Default::default() }).unwrap()
        }
        let cx = mcx::MemoryContext::new_bump("keysem alias test");
        let m = cx.mcx();
        let param = |id: i32| param(m, id);
        let relabel = |n| relabel(m, n);
        let func = |args: &[_], ret: u32, coll: u32| call(m, args, ret, coll);
        assert_eq!(alias::one_call(func(&[param(2), param(1)], 23, 0), 23), Some((2189, [1, 0])));
        assert_eq!(alias::one_call(func(&[param(1), relabel(param(2))], 23, 0), 23), Some((2189, [0, 1])));
        let op = Node::mk(m, OpExpr { opno: 413, opfuncid: 469, opresulttype: 23, args: NodeList::from_slice(m, &[param(2), param(1)]).unwrap(), ..Default::default() }).unwrap();
        assert_eq!(alias::one_call(op, 23), Some((469, [1, 0])));
        assert_eq!(alias::one_call(func(&[param(1), param(1)], 23, 0), 23), None, "one argument twice");
        assert_eq!(alias::one_call(func(&[param(2), param(1)], 16, 0), 23), None, "another result type");
        assert_eq!(alias::one_call(func(&[param(2), param(1)], 23, 100), 23), None, "an input collation");
        assert_eq!(alias::one_call(func(&[param(1)], 23, 0), 23), None, "one argument");
        assert_eq!(alias::one_call(func(&[param(2), func(&[param(1), param(2)], 23, 0)], 23, 0), 23), None, "a nested call");
        assert_eq!(alias::one_call(param(1), 23), None, "no call");
    }

    /// D3375 N7: the session sweep keeps a running binding (stale) and drops the rest.
    #[test]
    fn the_sweep_keeps_a_running_binding() {
        let id = crate::resolved::resolve(351).unwrap().unwrap();
        BOUND.with(|h| {
            let mut h = h.borrow_mut();
            h.clear();
            h.push(slot(id, None));
            h.push(slot(id, None));
        });
        let run = BOUND.with(|h| {
            let mut h = h.borrow_mut();
            h[1].run()
        });
        sweep();
        BOUND.with(|h| {
            let h = h.borrow();
            assert!(h.len() == 1 && h[0].busy() && h[0].runs(&run), "the running binding stays");
        });
        drop(run);
        BOUND.with(|h| assert!(h.borrow().is_empty()));
    }
}
