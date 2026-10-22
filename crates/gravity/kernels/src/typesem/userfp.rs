//! USEROP (D1604 / D1889 serve): `m6_user_fp`, the exact image of a user key (gravity_fn typesem keysem/user.rs): the Fp128 of
//! the value's class under its type's own `=`, found by the type's support functions through fmgr. A class is one
//! representative (a detoasted copy) and its number: with a hash proc, the value's hash picks a bucket and the equality
//! proc confirms each representative there (PostgreSQL's hashed grouping: buckets by the hash function, compares by the
//! operator); with only a btree comparison, a binary search over the representatives in its order finds the one it calls
//! equal (its sorted grouping). A value no representative matches founds a new class. Equal values share a class and
//! unequal ones never do (the opclass contracts), so the keyed tables keep deciding a hit by the image alone.
//!
//! D4197: the classes are the run's (`RunKeys`, protocol `keys.user`: exec ops/shared/user_keys.rs makes one per execution
//! behind the program's handle word and drops it with the execution, an error's end included), one table per spec, made
//! at the spec's first value. A class number never changes within the run, so no pipeline of it (an InitPlan's, a rerun
//! set's, a LAZY hashed set's, a rewound cursor's) sees an image move; no other run reads them, so a group shows a member
//! of its own run's rows (PostgreSQL's table is its node's, per execution), and a nested statement is a run with classes
//! of its own. DDL that could change the type's `=` gives later lowerings a new spec (its stamp of what it read), so a
//! table and its bound calls never outlive the catalog they were made under, except for a run in flight across the DDL. A
//! support function's call count is free (DECISIONS 6); it runs as PostgreSQL's does, under the type's collation, its error
//! stashed for the caller. The bound calls (their `fn_extra`, their result contexts) and the representatives are the run's
//! memory, freed with it. A run answers only the thread that made it (the leader's): another thread's lookup is an error
//! by name, never a shared table.
//!
//! D3374 (e) (record keys E1): a per-row record key (keysem `PER_ROW`: an anonymous record or `record[]` whose columns
//! are known only per row) holds values of several layouts in one table. `record_eq` (`array_eq` over record elements)
//! raises 42804 on two layouts; here such a pair is two classes, and the raise is kept (`Run::collided`). A class's
//! bucket is its values' whole hash_record / hash_array (`class_of`), so a kept pair hashes alike. PostgreSQL's
//! RecursiveUnion table compares two rows only when the whole row hashes alike: the IR asks the union's set
//! (`UserCollideProbe`) whether it holds this row with a colliding class in the key's place (every other key column
//! equal), and raises the kept 42804 only then, as PostgreSQL does; any other pair is one PostgreSQL never compares.
use crate::host_abi::*;
use gravity_fn::arena::{Charge, Door, Kind, OpArena, Owner, Release};
use gravity_fn::typesem::keysem::user::UserKey;
use gravity_fn::{Datum, FmgrCall, PgError};

struct Classes {
    spec: u64,
    key: UserKey,
    eq: FmgrCall,
    hash: Option<FmgrCall>,
    cmp: Option<FmgrCall>,
    calls: u32,
    /// Each class's representative, 8-byte aligned (a varlena behind a 4-byte header, a fixed value's bytes, or a by-value
    /// key's datum word).
    reps: Vec<Box<[u64]>>,
    /// hash value -> the classes in that bucket (a hash proc's road).
    buckets: std::collections::HashMap<u32, Vec<u32>>,
    /// The classes in the comparison's order (the comparison's road).
    sorted: Vec<u32>,
    /// D3374 (e): a per-row record key's (keysem `PER_ROW`): a 42804 from the equality is two classes, never a raise.
    per_row: bool,
    /// CONVERT wave 2 (site 13): the heap bytes the representatives (their boxes and vector), the buckets' vectors and
    /// `sorted` hold, kept where they grow (`Classes::bytes` adds the bucket table's block and the object).
    heap: u64,
}

/// PostgreSQL's ERRCODE_DATATYPE_MISMATCH: `record_eq` over two records of dissimilar column types or counts.
const DATATYPE_MISMATCH: i32 = crate::contract::sqlstate(*b"42804");

/// `keys.user`'s object while it lives (`Run::magic`): a null, stale or foreign handle is an error by name.
pub const KEYS_MAGIC: u64 = 0x8a11_7e11_0000_0601;

thread_local! {
    /// This thread's mark (its address): a run's classes answer only the thread that made them.
    static HERE: u8 = const { 0 };
}

/// An entry's size, read off the collection's own element type.
fn entry<'a, T: 'a>(_: impl IntoIterator<Item = &'a T>) -> usize {
    core::mem::size_of::<T>()
}

fn here() -> usize {
    HERE.with(|h| h as *const u8 as usize)
}

/// Each run's serial, from 1 (0 is no run's): a hashed IN list's set keeps the serial of the run that classed its
/// elements (lowerv4 step/raw/saop_set.rs), so a set an admission keeps for its next execution is classed again there.
static SERIALS: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(1);

/// One run's user-key classes (protocol `keys.user`), owned: exec makes it per execution on the thread that runs it, lays
/// its handle into the program's word and drops it with the execution.
pub struct RunKeys(core::ptr::NonNull<Run>);

/// The object behind a `RunKeys` handle. `specs`: each spec's table, made at its first value (out of its slot while the
/// spec's support functions run); `collided` / `armed`: the row's per-row key collisions (D3374 (e)).
struct Run {
    /// CONVERT wave 2 (site 13): the classes' bytes (`Run::bytes`) charged to the owner the first call with a ctx gives
    /// (`own`: the breaker its pipeline feeds, else the statement), `charged` of them committed: re-synced after a lookup
    /// once the lag reaches `SYNC_SLACK`. Declared first, so dropped first (door 1: the bytes move to the freeing thread's
    /// account, where the frees land).
    charge: Option<Charge>,
    charged: u64,
    owned: bool,
    magic: u64,
    here: usize,
    serial: u64,
    specs: Vec<(u64, Option<Box<Classes>>)>,
    collided: Vec<Collided>,
    /// The collisions are kept only between `UserCollideClear` (on) and `UserCollideProbe` (off): a recursive UNION's row
    /// image. Any other lookup (record keys E2's sorted group, whose Sort raised first) takes such a pair as two classes.
    armed: bool,
}

impl RunKeys {
    /// A run's classes, none yet, answering this thread.
    #[allow(clippy::new_without_default)]
    pub fn new() -> RunKeys {
        let serial = SERIALS.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
        let run = Box::new(Run { charge: None, charged: 0, owned: false, magic: KEYS_MAGIC, here: here(), serial, specs: Default::default(), collided: Default::default(), armed: false }); // alloc: Exec
        RunKeys(core::ptr::NonNull::from(Box::leak(run)))
    }

    /// CONVERT wave 2 (site 13): the owner the run's classes are charged to from now (the statement's, Kind::Region, from
    /// its maker, exec user_keys.rs, once the driver's stmt_context is in reach there); unset, the first call with a ctx sets it.
    pub fn own(&self, owner: Option<(Owner, Kind)>) {
        // SAFETY: the object this owner made, alive until its drop; set on the run's thread between its calls
        let run = unsafe { &mut *self.0.as_ptr() };
        if !run.owned {
            run.own(owner);
        }
    }

    /// The handle word the program's IR passes (the object's address).
    pub fn handle(&self) -> u64 {
        self.0.as_ptr() as u64
    }

    /// The classes the run holds, over every spec.
    pub fn classes(&self) -> usize {
        // SAFETY: the object this owner made, alive until its drop; read on the run's thread between its calls
        let run = unsafe { self.0.as_ref() };
        run.specs.iter().filter_map(|(_, c)| c.as_ref()).map(|c| c.reps.len()).sum()
    }
}

/// The most a run's charge lags its classes' bytes between two syncs (RULINGS-1 R1: batched, bounded, stated).
const SYNC_SLACK: u64 = 64 << 10;

impl Run {
    /// The owner the classes are charged to from now, with what they hold already (`None`: no one).
    fn own(&mut self, owner: Option<(Owner, Kind)>) {
        self.owned = true;
        if let Some((o, k)) = owner {
            self.charge = Some(Charge::open(o, Release::Statement, k, Door::Heap, 0));
            self.sync(0);
        }
    }

    /// The heap bytes the run's classes hold (a spec's classes out of their slot during its call count at the put-back).
    fn bytes(&self) -> u64 {
        let specs = (self.specs.capacity() * entry(&self.specs)) as u64;
        specs + self.specs.iter().filter_map(|(_, c)| c.as_ref()).map(|c| c.bytes()).sum::<u64>()
    }

    /// The charge re-synced to `bytes` once the classes grew `slack` bytes or more past it (classes only grow until the
    /// run drops).
    fn sync(&mut self, slack: u64) {
        let b = self.bytes();
        if let Some(c) = self.charge.as_mut().filter(|_| b >= self.charged + slack.max(1)) {
            c.commit(b - self.charged);
            self.charged = b;
        }
    }
}

/// CONVERT wave 2 (site 13): run `h` charged to the owner of `ctx`'s pipeline from its first call with a ctx (a call with
/// none, the hashed ScalarArrayOp's `class_number`, leaves it as it is).
/// # Safety
/// As `run_of`; `ctx` is null or the caller's bound ctx.
unsafe fn own_from(h: u64, ctx: *mut Ctx) {
    if let Ok(run) = run_of(h) {
        if !(*run).owned && !ctx.is_null() {
            (*run).own(OpArena::slot_owner_kind(ctx));
        }
    }
}

impl Drop for RunKeys {
    fn drop(&mut self) {
        // SAFETY: the object `new` leaked, freed once here; no call of the run is on the stack (its execution ended)
        unsafe {
            (*self.0.as_ptr()).magic = 0;
            drop(Box::from_raw(self.0.as_ptr()));
        }
    }
}

/// The run behind handle `h`, made on this thread; an error by name otherwise.
/// # Safety
/// `h` is 0, a live `RunKeys`'s handle, or the address of another object of at least one readable word.
unsafe fn run_of(h: u64) -> Result<*mut Run, Box<PgError>> {
    let run = h as usize as *mut Run;
    if run.is_null() || (*run).magic != KEYS_MAGIC {
        return Err(err(&format!("a stale or foreign keys.user handle {h:#x}"))); // alloc: Cold
    }
    if (*run).here != here() {
        return Err(err("a run's classes reached from another thread than the run's"));
    }
    Ok(run)
}

/// Reset each bound call's result context every so many calls (a support function's detoasted argument copies are its).
const WINDOW: u32 = 1024;

fn err(what: &str) -> Box<PgError> {
    Box::new(PgError::error(format!("gravity: rep.hosted.hash user key: {what}"))) // alloc: Cold
}

impl Classes {
    fn new(spec: u64) -> Result<Classes, Box<PgError>> {
        let key = gravity_fn::typesem::keysem::user::spec(spec)?;
        let bind = |id: u32, n: usize| FmgrCall::bind_resolved(id, n, key.coll).ok_or_else(|| err("a support function no lowering resolved"));
        let eq = bind(key.eq, 2)?;
        let hash = if key.hash != 0 { Some(bind(key.hash, 1)?) } else { None };
        let cmp = if key.hash == 0 { Some(bind(key.cmp, 2)?) } else { None };
        let per_row = key.row == gravity_fn::typesem::keysem::user::PER_ROW;
        Ok(Classes { spec, key, eq, hash, cmp, calls: 0, reps: Default::default(), buckets: Default::default(), sorted: Default::default(), per_row, heap: 0 }) // alloc: Exec
    }

    fn tick(&mut self) {
        self.calls = self.calls.wrapping_add(1);
        if self.calls % WINDOW == 0 {
            self.eq.reset_window();
            if let Some(h) = self.hash.as_mut() {
                h.reset_window();
            }
            if let Some(c) = self.cmp.as_mut() {
                c.reset_window();
            }
        }
    }

    /// The representative of `class` as its support functions take it: a by-value key's word, else its copy's address.
    fn rep(&self, class: u32) -> usize {
        let r = &self.reps[class as usize];
        if self.key.byval {
            r[0] as usize
        } else {
            r.as_ptr() as usize
        }
    }

    /// `armed`: a 42804 pair of a per-row key is kept in `kept` (the run's `armed`, read before the lookup).
    fn equal(&mut self, v: usize, class: u32, armed: bool, kept: &mut Vec<Collided>) -> Result<bool, Box<PgError>> {
        self.tick();
        let r = self.rep(class);
        let out = match self.eq.call_site(&[Datum::from_usize(v), Datum::from_usize(r)], &[false, false]) {
            Ok(out) => out,
            // D3374 (e): two layouts of a per-row record key: two classes, the raise kept for the row's probe (the module's note)
            Err(e) if self.per_row && e.sqlstate().0 == DATATYPE_MISMATCH => {
                if armed {
                    kept.push(Collided { spec: self.spec, own: None, other: class, e }); // alloc: Cold
                }
                return Ok(false);
            }
            Err(e) => return Err(e),
        };
        Ok(!out.isnull && out.value.as_bool())
    }

    fn order(&mut self, v: usize, class: u32) -> Result<core::cmp::Ordering, Box<PgError>> {
        self.tick();
        let r = self.rep(class);
        let c = self.cmp.as_mut().expect("the comparison's road");
        let out = c.call_site(&[Datum::from_usize(v), Datum::from_usize(r)], &[false, false])?;
        if out.isnull {
            return Err(err("a btree comparison returned NULL"));
        }
        Ok(out.value.as_i32().cmp(&0))
    }

    /// A copy of the value `v` that outlives its row: a by-value key's word itself; at the address `v`, a varlena
    /// detoasted behind a 4-byte header or a fixed value's bytes.
    unsafe fn copy(&self, v: usize) -> Result<Box<[u64]>, Box<PgError>> {
        if self.key.byval {
            return Ok(vec![v as u64].into_boxed_slice()); // alloc: Exec
        }
        let bytes: Vec<u8> = if self.key.len == -1 {
            gravity_fn::with_payload(v as *const u8, |p| {
                let mut b = Vec::with_capacity(p.len() + 4); // alloc: Exec
                b.extend_from_slice(&(((p.len() + 4) as u32) << 2).to_le_bytes());
                b.extend_from_slice(p);
                b
            })?
        } else {
            core::slice::from_raw_parts(v as *const u8, self.key.len as usize).to_vec() // alloc: Exec
        };
        let mut w = vec![0u64; bytes.len().div_ceil(8).max(1)].into_boxed_slice(); // alloc: Exec
        core::ptr::copy_nonoverlapping(bytes.as_ptr(), w.as_mut_ptr() as *mut u8, bytes.len());
        Ok(w)
    }

    unsafe fn found(&mut self, v: usize) -> Result<u32, Box<PgError>> {
        let class = self.reps.len() as u32;
        let rep = self.copy(v)?;
        let (cap, add) = (self.reps.capacity(), rep.len() * 8);
        self.reps.push(rep); // alloc: Exec charged the run's charge (Run::sync)
        self.heap += (add + (self.reps.capacity() - cap) * entry(&self.reps)) as u64;
        Ok(class)
    }

    /// The heap bytes the classes hold: `heap`, the bucket table's block (hashbrown: its buckets, a power of two holding the
    /// capacity at 7 / 8, x (entry + a control byte) + a group of control bytes) and the boxed object.
    fn bytes(&self) -> u64 {
        let table = match self.buckets.capacity() {
            0 => 0,
            c if c < 8 => (c + 1).next_power_of_two() * (core::mem::size_of::<(u32, Vec<u32>)>() + 1) + 16,
            c => (c * 8 / 7).next_power_of_two() * (core::mem::size_of::<(u32, Vec<u32>)>() + 1) + 16,
        };
        self.heap + (table + core::mem::size_of::<Classes>()) as u64
    }

    /// The class of the value `v` (a live datum of the key's type, not NULL: a by-value key's word, else its address).
    unsafe fn class_of(&mut self, v: usize, armed: bool, kept: &mut Vec<Collided>) -> Result<u32, Box<PgError>> {
        if self.hash.is_some() {
            self.tick();
            let h = self.hash.as_mut().expect("the hash proc's road").call_site(&[Datum::from_usize(v)], &[false])?;
            let h = if h.isnull { 0 } else { h.value.as_i32() as u32 };
            let n = self.buckets.get(&h).map_or(0, Vec::len);
            let mut hit = None;
            for i in 0..n {
                let class = self.buckets[&h][i];
                if self.equal(v, class, armed, kept)? {
                    hit = Some(class);
                    // D3374 (e): a per-row key meets every class of its hash, so each one of another layout is kept
                    // (`collided`) whichever order the bucket holds them in
                    if !self.per_row {
                        break;
                    }
                }
            }
            if let Some(class) = hit {
                return Ok(class);
            }
            let class = self.found(v)?;
            let b = self.buckets.entry(h).or_default();
            let cap = b.capacity();
            b.push(class); // alloc: Exec charged the run's charge (Run::sync)
            self.heap += ((b.capacity() - cap) * 4) as u64;
            return Ok(class);
        }
        let (mut lo, mut hi) = (0usize, self.sorted.len());
        while lo < hi {
            let mid = (lo + hi) / 2;
            match self.order(v, self.sorted[mid])? {
                core::cmp::Ordering::Equal => return Ok(self.sorted[mid]),
                core::cmp::Ordering::Less => hi = mid,
                core::cmp::Ordering::Greater => lo = mid + 1,
            }
        }
        let class = self.found(v)?;
        let cap = self.sorted.capacity();
        self.sorted.insert(lo, class); // alloc: Exec charged the run's charge (Run::sync)
        self.heap += ((self.sorted.capacity() - cap) * 4) as u64;
        Ok(class)
    }
}

/// D2125 (lowerv4 expr.saop: a hashed ScalarArrayOp over a user key): the class number of the live, non-NULL value `v` (a
/// by-value key's word, else its address) under user key `spec` in run `h`, found as `m6_user_fp` finds it (the hash proc,
/// then the equality against the bucket's representatives): equal values under the type's `=` share it, unequal ones
/// never do.
/// # Safety
/// `v` is a live datum of the spec's key type; `h` is as `run_of` takes it.
pub unsafe fn class_number(h: u64, v: usize, spec: u64) -> Result<u32, Box<PgError>> {
    class(h, v, spec).map(|(c, _)| c)
}

/// The serial of run `h` (never 0): a set classed under another serial is classed again.
/// # Safety
/// As `run_of`.
pub unsafe fn run_serial(h: u64) -> Result<u64, Box<PgError>> {
    run_of(h).map(|run| (*run).serial)
}

/// The class of `v` under user key `spec` in run `h`, and its representative's first word (a by-value key's datum: its
/// exact one-word image). The spec's classes leave the run's table while its support functions run: a lookup that finds
/// them out is the run re-entered from inside one, an error by name (a statement those functions run is a run of its
/// own, with its own classes).
/// # Safety
/// As `class_number`.
unsafe fn class(h: u64, v: usize, spec: u64) -> Result<(u32, u64), Box<PgError>> {
    let run = run_of(h)?;
    let (mut cl, armed) = {
        let r = &mut *run;
        let cl = match r.specs.iter().position(|(s, _)| *s == spec) {
            Some(i) => r.specs[i].1.take().ok_or_else(|| err("a run's classes re-entered from inside one of their support functions"))?,
            None => {
                let cl = Box::new(Classes::new(spec)?); // alloc: Exec
                r.specs.push((spec, None)); // alloc: Exec
                cl
            }
        };
        (cl, r.armed)
    };
    let mut kept = Vec::new(); // alloc: Cold
    let found = cl.class_of(v, armed, &mut kept).map(|class| (class, cl.reps[class as usize][0]));
    let r = &mut *run;
    if let Some(slot) = r.specs.iter_mut().find(|(s, _)| *s == spec) {
        slot.1 = Some(cl);
    }
    r.sync(SYNC_SLACK);
    if !kept.is_empty() {
        // D3374 (e): the collisions this lookup kept are its class's
        if let Ok((class, _)) = found {
            kept.iter_mut().for_each(|x| x.own = Some(class));
        }
        r.collided.append(&mut kept);
    }
    found
}

/// `m6_user_fp` (lowerv4 rep.hosted.hash, USEROP; `contract::entry4::<UserFp>`): `(h, datum, spec, out)`, the exact Fp128
/// of the class of the key value at `datum` (a varlena in any header form, or a fixed by-reference value's bytes) under
/// user key `spec` (typesem `user_key`: the program's, from its lowering) in run `h` (the program's `keys.user` word) at
/// `out[0..2]`; answers 0. ERROR (stashed): a support function's error, a spec no lowering built, a detoast failure, no
/// fingerprint seed, or a handle `run_of` refuses. A panic answers ERROR.
pub struct UserFp;

impl crate::contract::Host for UserFp {
    type Args = (u64, u64, u64, u64);
    /// # Safety
    /// `datum` is a live value of the key's type, `out` is writable for two words; `h` is as `run_of` takes it.
    unsafe fn run(ctx: *mut Ctx, (h, datum, spec, out): (u64, u64, u64, u64)) -> u64 {
        let out = out as usize as *mut u64;
        own_from(h, ctx);
        match class(h, datum as usize, spec).and_then(|(class, _)| fp_of(spec, class)) {
            Ok(fp) => {
                out.write_unaligned(fp[0]);
                out.add(1).write_unaligned(fp[1]);
                0
            }
            Err(e) => {
                gravity_fn::stash_error(e);
                ERROR as u64
            }
        }
    }
}

/// `m6_user_class` (lowerv4 rep.word.hash, D2723; `contract::entry4::<UserClass>`): `(h, datum, spec, out)`, the datum word
/// of the class representative of the by-value key word `datum` under user key `spec` in run `h` (typesem `user_key` of an
/// enum or other by-value type whose `=` is user code) at `out[0]`: an exact one-word image (equal values share the
/// representative, unequal ones never do) that is also a member of the group, so a keyed form that renders the key from
/// its word shows a value equal to every member (the equal-member ruling), one of this run's rows; answers 0. ERROR
/// (stashed): a support function's error, a spec no lowering built, or a handle `run_of` refuses. A panic answers ERROR.
pub struct UserClass;

impl crate::contract::Host for UserClass {
    type Args = (u64, u64, u64, u64);
    /// # Safety
    /// `datum` is a live by-value datum of the key's type, `out` is writable for one word; `h` is as `run_of` takes it.
    unsafe fn run(ctx: *mut Ctx, (h, datum, spec, out): (u64, u64, u64, u64)) -> u64 {
        own_from(h, ctx);
        match class(h, datum as usize, spec) {
            Ok((_, word)) => {
                (out as usize as *mut u64).write_unaligned(word);
                0
            }
            Err(e) => {
                gravity_fn::stash_error(e);
                ERROR as u64
            }
        }
    }
}

/// The exact Fp128 of class `class` of user key `spec` (`m6_user_fp`'s image).
fn fp_of(spec: u64, class: u32) -> Result<[u64; 2], Box<PgError>> {
    let keys = gravity_dict::fp::keys().ok_or_else(|| err("a fingerprint seed before it is installed"))?;
    let mut b = [0u8; 12];
    b[..8].copy_from_slice(&spec.to_le_bytes());
    b[8..].copy_from_slice(&class.to_le_bytes());
    Ok(keys.fp_bytes(&b))
}

/// D3374 (e) (record keys E1; lowerv4 ops/setop/recursive.rs `Dedup::fresh`, `contract::entry1::<UserCollideClear>`):
/// `(h,)`, run `h`'s kept per-row key collisions dropped and kept again from here, before a row's image; answers 0, or
/// ERROR (stashed) for a handle `run_of` refuses.
pub struct UserCollideClear;

impl crate::contract::Host for UserCollideClear {
    type Args = (u64,);
    /// # Safety
    /// `h` is as `run_of` takes it.
    unsafe fn run(_ctx: *mut Ctx, (h,): (u64,)) -> u64 {
        match run_of(h) {
            Ok(run) => {
                (*run).collided.clear();
                (*run).armed = true;
                0
            }
            Err(e) => {
                gravity_fn::stash_error(e);
                ERROR as u64
            }
        }
    }
}

/// D3374 (e): a pair `record_eq` / `array_eq` raised 42804 on: user key `spec`'s value of class `own` (`None` when its
/// lookup failed) against class `other`'s representative, and that error.
struct Collided {
    spec: u64,
    own: Option<u32>,
    other: u32,
    e: Box<PgError>,
}

/// D3374 (e) (record keys E1; lowerv4 ops/setop/recursive.rs `Dedup::fresh`, `contract::entry4::<UserCollideProbe>`):
/// `(keys, h, img, desc)`, after the row's image (`1 + n` words at `img`: its KeyHash, then the `n` image words) and before
/// its offer: `desc` addresses `[n, k, off_1 .. off_k]`, the image word each per-row key's two words start at. A key whose
/// words are class `own`'s Fp128 under one of run `keys`'s kept collisions may hold any class `own` collided with instead;
/// for each such row but the row itself (every other key column equal), whether recursive union `h`'s set holds it: the
/// row PostgreSQL's table compares this one with, whose compare raised. Answers 0 when none is held; ERROR (stashed) with
/// the kept 42804 of the last key column substituted (PostgreSQL compares the columns last to first) when one is, or with
/// the set's own error, a bad handle or a bad descriptor.
pub struct UserCollideProbe;

impl crate::contract::Host for UserCollideProbe {
    type Args = (u64, u64, u64, u64);
    /// # Safety
    /// `keys` is as `run_of` takes it; `h` is the union's generation object handle (its magic refuses a stale one by
    /// name); `img` addresses `1 + n` readable words and `desc` `2 + k` for the call (the IR's stack slots).
    unsafe fn run(ctx: *mut Ctx, (keys, h, img, desc): (u64, u64, u64, u64)) -> u64 {
        use crate::ops::setop::entries::{GenCell, GEN_MAGIC, OP_GEN_HAS};
        let run = match run_of(keys) {
            Ok(run) => run,
            Err(e) => {
                gravity_fn::stash_error(e);
                return ERROR as u64;
            }
        };
        (*run).armed = false;
        let mut kept = core::mem::take(&mut (*run).collided);
        if kept.is_empty() {
            return 0;
        }
        let cell = h as *mut GenCell;
        let (n, k) = if desc == 0 { (0, 0) } else { (*(desc as usize as *const u64) as usize, *(desc as usize as *const u64).add(1) as usize) };
        let offs = if desc == 0 { &[][..] } else { core::slice::from_raw_parts((desc as usize as *const u64).add(2), k) };
        if cell.is_null() || (*cell).head.magic != GEN_MAGIC || img == 0 || offs.iter().any(|&o| o as usize + 2 > n) {
            gravity_fn::stash_error(err(&format!("m6_user_probe: a stale handle {h:#x} or an image of {n} words with keys at {offs:?}"))); // alloc: Cold
            return ERROR as u64;
        }
        let row: &[u64] = core::slice::from_raw_parts(img as usize as *const u64, 1 + n);
        // each per-row key's choices: its own words, then each class its own class collided with (and that error). One flat
        // list with the start of each key's run, on the breaker slot `ctx`'s pipeline produces into, else the statement's
        // (`OpArena::slot_owner_kind`), asked only here (S6-body: no escape, no live raw pointer into these buffers, checked)
        let o = gravity_fn::arena::OpArena::slot_owner_kind(ctx);
        let (ow, kd) = (o.map(|o| o.0), o.map_or(gravity_fn::arena::Kind::Region, |o| o.1));
        let mut flat = gravity_fn::arena::ChargedVec::<([u64; 2], Option<usize>)>::new(ow, kd);
        let mut starts = gravity_fn::arena::ChargedVec::<usize>::new(ow, kd);
        starts.with_vec(|v| v.push(0)); // alloc: Exec charged o
        for &o in offs {
            let o = o as usize;
            let mine = [row[1 + o], row[2 + o]];
            flat.with_vec(|v| v.push((mine, None))); // alloc: Exec charged o
            for (i, x) in kept.iter().enumerate() {
                let Some(own) = x.own else { continue };
                match (fp_of(x.spec, own), fp_of(x.spec, x.other)) {
                    (Ok(fp), Ok(alt)) if fp == mine => flat.with_vec(|v| v.push((alt, Some(i)))), // alloc: Exec charged o
                    (Ok(_), Ok(_)) => {}
                    (Err(e), _) | (_, Err(e)) => {
                        gravity_fn::stash_error(e);
                        return ERROR as u64;
                    }
                }
            }
            let end = flat.len();
            starts.with_vec(|v| v.push(end)); // alloc: Exec charged o
        }
        // every combination but all-own, the first key's choice slowest. `alt` stays a plain Vec: its pointer is handed to the
        // set's op below (S6: host ABI)
        let mut alt: Vec<u64> = row.to_vec(); // alloc: Cold
        let mut at = gravity_fn::arena::ChargedVec::<usize>::new(ow, kd);
        at.with_vec(|v| v.resize(k, 0)); // alloc: Exec charged o
        loop {
            let Some(j) = (0..k).rev().find(|&j| at[j] + 1 < starts[j + 1] - starts[j]) else { return 0 };
            at[j] += 1;
            at[j + 1..].iter_mut().for_each(|a| *a = 0);
            let mut last = None;
            for (j, &o) in offs.iter().enumerate() {
                let (w, e) = flat[starts[j] + at[j]];
                alt[1 + o as usize] = w[0];
                alt[2 + o as usize] = w[1];
                last = e.or(last);
            }
            alt[0] = gravity_abi::hash::KeyHash::of(&alt[1..]).to_abi();
            let head = core::ptr::addr_of_mut!((*cell).head);
            match ((*cell).head.op)(head, ctx, OP_GEN_HAS, alt.as_ptr() as u64, n as u64) {
                0 => {}
                1 => {
                    let Some(i) = last else { return 0 };
                    let e = core::mem::replace(&mut kept[i].e, err("m6_user_probe: a kept error raised twice"));
                    gravity_fn::stash_error(e);
                    return ERROR as u64;
                }
                st => return st as u64,
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `float8eq` and `hashfloat8` (pg_proc.dat): `-0` and `0` are equal under them, two words.
    const FLOAT8EQ: u32 = 293;
    const HASHFLOAT8: u32 = 452;
    const FLOAT8OID: u32 = 701;
    /// A spec no lowering interned: a run finds only the table the test gives it.
    const SPEC: u64 = 0x7e57_0000_0000_0001;

    fn run(k: &RunKeys) -> *mut Run {
        k.0.as_ptr()
    }

    /// float8 under its own `=` as a by-value user key in run `k`, its calls bound as `Classes::new` binds a spec's (a
    /// builtin resolves with no catalog).
    fn with_float8(k: &RunKeys) {
        let id = |p: u32| gravity_fn::resolved::resolve(p).expect("a builtin resolves").expect("not refused");
        let key = UserKey { typid: FLOAT8OID, eq: id(FLOAT8EQ), hash: id(HASHFLOAT8), cmp: 0, coll: 0, len: 8, byval: true, stamp: 1, row: 0, site: 0 };
        let bind = |p: u32, n: usize| FmgrCall::bind_resolved(p, n, 0).expect("bound");
        let cl = Box::new(Classes { spec: SPEC, key, eq: bind(key.eq, 2), hash: Some(bind(key.hash, 1)), cmp: None, calls: 0, reps: Vec::new(), buckets: std::collections::HashMap::new(), sorted: Vec::new(), per_row: false, heap: 0 }); // alloc: Cold
        // SAFETY: the test's own run, no call of it on the stack
        unsafe { (*run(k)).specs.push((SPEC, Some(cl))) }; // alloc: Cold
    }

    /// `m6_user_class` through its entry: the representative word of `v` in run `h`, or the stashed error's message.
    fn rep_word(h: u64, v: f64) -> Result<u64, String> {
        let mut out = 0u64;
        // SAFETY: a by-value float8 word and one writable word
        let st = unsafe { <UserClass as crate::contract::Host>::run(core::ptr::null_mut(), (h, v.to_bits(), SPEC, &mut out as *mut u64 as u64)) };
        match st {
            0 => Ok(out),
            _ => Err(gravity_fn::take_error().map(|e| e.message().to_string()).unwrap_or_default()), // alloc: Cold
        }
    }

    /// D4197: a later run's group shows a member of its own rows. Run 1 classes `-0`; run 2 meets only `0`, equal to it,
    /// and shows `0`. Run 2 never finds run 1's table: with none of its own, its first lookup makes one (here it cannot: the
    /// test's spec was never interned), where a thread's table kept for the session answered `-0`.
    #[test]
    fn a_specs_classes_do_not_outlive_their_run() {
        let one = RunKeys::new();
        with_float8(&one);
        assert_eq!(rep_word(one.handle(), -0.0), Ok((-0.0f64).to_bits()));
        assert_eq!(rep_word(one.handle(), 0.0), Ok((-0.0f64).to_bits()), "0 = -0 in the run that classed -0 first");
        assert_eq!(one.classes(), 1);
        drop(one);
        let bare = RunKeys::new();
        assert!(rep_word(bare.handle(), 0.0).is_err_and(|m| m.contains("was not built in this process")), "a new run makes its own table");
        let two = RunKeys::new();
        with_float8(&two);
        assert_eq!(rep_word(two.handle(), 0.0), Ok(0.0f64.to_bits()), "run 2's group shows its own member");
        assert_eq!(rep_word(two.handle(), -0.0), Ok(0.0f64.to_bits()));
        // SAFETY: the test's own runs, no call of them on the stack
        assert_ne!(unsafe { (*run(&bare)).serial }, unsafe { (*run(&two)).serial });
    }

    /// CONVERT wave 2 (site 13): the classes' running heap bytes equal a full recount as they grow (the run's charge follows
    /// them), and a call with no ctx leaves the run unowned.
    #[test]
    fn the_running_class_bytes_equal_a_recount() {
        let k = RunKeys::new();
        with_float8(&k);
        for i in 0..5000 {
            assert!(rep_word(k.handle(), i as f64).is_ok());
        }
        assert_eq!(k.classes(), 5000);
        // SAFETY: the test's own run, no call of it on the stack
        let r = unsafe { &*run(&k) };
        let cl = r.specs[0].1.as_ref().expect("put back");
        let reps = cl.reps.capacity() * core::mem::size_of::<Box<[u64]>>() + cl.reps.iter().map(|b| b.len() * 8).sum::<usize>();
        let buckets = cl.buckets.values().map(|v| v.capacity() * 4).sum::<usize>();
        assert_eq!(cl.heap, (reps + buckets + cl.sorted.capacity() * 4) as u64);
        assert!(r.bytes() > cl.heap + 5000 * 8 && !r.owned && r.charge.is_none() && r.charged == 0);
    }

    /// A null, stale or foreign handle is an error by name, never a read through it.
    #[test]
    fn a_stale_or_foreign_handle_is_an_error_by_name() {
        assert!(rep_word(0, 1.0).is_err_and(|m| m.contains("a stale or foreign keys.user handle")));
        let foreign = Box::new([0x8a11_7e11_0000_0005u64, 0, 0, 0]); // alloc: Cold
        assert!(rep_word(foreign.as_ptr() as u64, 1.0).is_err_and(|m| m.contains("a stale or foreign keys.user handle")));
    }

    /// A run answers only the thread that made it: another thread's lookup is an error by name (a pool thread reaching a
    /// user key, D4193's road, never touches the leader's table).
    #[test]
    fn another_threads_lookup_is_an_error_by_name() {
        let k = RunKeys::new();
        with_float8(&k);
        let h = k.handle();
        let there = std::thread::spawn(move || rep_word(h, 1.0)).join().expect("the thread ran");
        assert_eq!(there, Err("gravity: rep.hosted.hash user key: a run's classes reached from another thread than the run's".to_string())); // alloc: Cold
        assert_eq!(k.classes(), 0);
    }

    /// A lookup that finds its spec's classes out of the table (taken by a lookup still running its support functions) is
    /// the run re-entered: an error by name, never a second table.
    #[test]
    fn a_reentered_run_is_an_error_by_name() {
        let k = RunKeys::new();
        // SAFETY: the test's own run, no call of it on the stack
        unsafe { (*run(&k)).specs.push((SPEC, None)) }; // alloc: Cold
        assert!(rep_word(k.handle(), 1.0).is_err_and(|m| m.contains("re-entered")));
    }

    /// D3374 (e): the collisions are the run's: a clear arms them, the probe disarms and takes them.
    #[test]
    fn the_collisions_are_the_runs() {
        let k = RunKeys::new();
        // SAFETY: a live run's handle; no descriptor (no per-row key), so the probe reads no image
        unsafe {
            assert_eq!(<UserCollideClear as crate::contract::Host>::run(core::ptr::null_mut(), (k.handle(),)), 0);
            assert!((*run(&k)).armed);
            assert_eq!(<UserCollideProbe as crate::contract::Host>::run(core::ptr::null_mut(), (k.handle(), 0, 0, 0)), 0);
        }
        // SAFETY: as above
        assert!(!unsafe { (*run(&k)).armed });
    }
}
