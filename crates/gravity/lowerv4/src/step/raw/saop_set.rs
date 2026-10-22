//! expr.saop's literal-free hashed set (V4R-102, V4R-122): the elements of a constant array PG's plan hashed
//! (`hashfuncid`), built once per execution at bind (exec admit.rs `WordTpl::Set` through the kind's `ExprExtMech::bind_set`) into a grant-pool region the execution holds (`GraphWords::sets`), its address the site's word; PG builds
//! its set once per expression state (execExprInterp.c `ExecEvalHashedScalarArrayOp`). The region holds the head, the
//! slots and a copy of the array's image, so every by-reference element the set holds lives as long as the set. One
//! open-addressing table of `[tag, value]` pairs: `tag` is the element's hash with bit 32 set (0 is an empty slot); `value`
//! the element's datum, or the sign-extended integer of a native set (typesem `native_eq`: its hash is the integer's, mixed),
//! or a user key's class number (D2125, `Desc::user`: classed at the site's first probe, since the user's functions reach
//! SPI, which bind cannot, in the run's classes, D4197; the pipeline is the leader's, so only the leader reads or writes
//! that set).
//! Workers read the others; nothing writes them after bind. Owner M2-FRAME.
#![allow(unsafe_code)]
use gravity_expr::nodes::array::Walk;
use gravity_expr::views::image_view;
use gravity_fn::{Datum, PgError, Region};

use crate::step::raw::saop_host::{call2, hash, native, owned_desc, with_region, Desc, Hashed};

const OCC: u64 = 1 << 32;

/// D2125 / D4197: from this bit of `Head::has_nulls`, the serial of the run whose classes a user key's set holds (kernels
/// userfp `run_serial`; 0: none yet). `probe` classes the elements at a run's first probe (PG builds its table at the
/// first evaluation, and the user's functions reach SPI, which bind cannot); an admission keeps its sets for its next
/// execution, whose run has classes of its own, so a probe under another serial classes them again. Bit 0 is the
/// array's NULL.
const RUN_AT: u32 = 8;

/// D2614: `Head::has_nulls`'s bit 2, an element whose hash raised at bind (a hash proc that reads its collation under an
/// indeterminate one: 42P22). PG hashes the elements at the first evaluation with a non-NULL operand
/// (`ExecEvalHashedScalarArrayOp`), never under EXPLAIN or over an empty scan: the set stays partial and `probe` raises
/// that hash's error at the first probe.
const HELD: u64 = 4;

/// `[tag, value]` into `slots` by open addressing (a native set keeps one slot per key).
fn insert(slots: &mut [[u64; 2]], tag: u64, value: u64, native: bool) {
    let mask = slots.len() - 1;
    let mut i = tag as usize & mask;
    while slots[i][0] != 0 && !(native && slots[i] == [tag, value]) {
        i = (i + 1) & mask;
    }
    slots[i] = [tag, value];
}

/// The set's head; `mask + 1` slots of `[tag, value]` follow it, then the array's image.
#[repr(C)]
struct Head {
    mask: u64,
    has_nulls: u64,
}

/// A native element's slot hash: the key hash of its word (gravity_abi::hash, the one mix).
fn mix(k: u64) -> u32 {
    gravity_abi::hash::KeyHash::word(k).stored()
}

fn err(m: &str) -> Box<PgError> {
    Box::new(PgError::error(format!("gravity: expr.saop: {m}"))) // alloc: Cold
}

/// D4367: a list's region (`what`: its set, its extreme) the grant pool or the allocator refused is the statement's out of
/// memory (53200), worded as exec glue words a refused breaker region, the detail naming what was refused; any other
/// driver error is `err`'s.
pub(crate) fn region_err(what: &str, e: gravity_driver::Error) -> Box<PgError> {
    match e {
        gravity_driver::Error::OutOfMemory { bytes, held, capacity, .. } => Box::new( // alloc: Cold
            PgError::error("out of memory".to_string()) // alloc: Cold
                .with_sqlstate(types_error::ERRCODE_OUT_OF_MEMORY)
                .with_detail(format!("Failed on request of size {bytes} in a Gravity region for {what} (grant pool: {held} of {capacity} bytes held).")) // alloc: Cold
                .with_hint("Lower work_mem or hash_mem_multiplier, or the number of concurrent statements.".to_string()), // alloc: Cold
        ),
        e => err(&format!("{what}: {e:?}")), // alloc: Cold
    }
}

/// The set of array `a` (a non-null datum of the node's array type) for the owned `Desc` `desc` of the program's host state
/// `owner`, in a region charged to the grant pool (`gravity_driver::in_list_set_region`): its address is the site's word for this
/// execution.
pub(crate) fn bound(owner: Option<&dyn core::any::Any>, desc: u32, a: u64) -> types_error::PgResult<gravity_driver::RegionBuf> {
    let owner = owner.map_or(core::ptr::null(), |o| o as *const dyn core::any::Any as *const u8);
    // SAFETY: `owner` is the program's host state (an `Owner`, `finish_keeping`'s or `rebuild`'s), borrowed for the call.
    let d = (!owner.is_null()).then(|| unsafe { owned_desc(owner, desc as usize) }).flatten().ok_or_else(|| err("the program owns no such site"))?;
    let h = d.hashed.as_ref().ok_or_else(|| err("a set word for a site PG did not hash"))?;
    if h.bytes {
        // LIST-FORM: a byte set (step/raw/saop_bytes.rs)
        // SAFETY: as `build`.
        return unsafe { crate::step::raw::saop_bytes::build_bytes(d, a) };
    }
    if h.extreme.is_some() {
        // LIST-EXT: the list's extreme (step/raw/saop_extreme.rs)
        // SAFETY: as `build`.
        return unsafe { crate::step::raw::saop_extreme::build(d, a) };
    }
    with_region(0, |region| unsafe { build(d, h, a, region) })
}

/// # Safety
/// `a` is a non-null datum of `d`'s array type.
unsafe fn build(d: &Desc, h: &Hashed, a: u64, region: &mut Region) -> types_error::PgResult<gravity_driver::RegionBuf> {
    let img = image_view(Datum::from_u64(a), -1, region)?;
    let n = Walk::over(Datum::from_usize(img.ptr as usize), d.elem, &mut Region::over(&mut []))?.len();
    let cap = (n * 2).next_power_of_two().max(8);
    let at = 16 + cap * 16;
    let mut buf = gravity_driver::in_list_set_region(at + img.len as usize).map_err(|e| region_err("the IN list's set", e))?;
    let base = buf.as_mut_ptr();
    if base as usize % 8 != 0 {
        return Err(err("the IN list's set region is not 8-aligned"));
    }
    core::ptr::copy_nonoverlapping(img.ptr, base.add(at), img.len as usize);
    let head = base as *mut Head;
    let slots = core::slice::from_raw_parts_mut(head.add(1) as *mut [u64; 2], cap);
    slots.fill([0, 0]);
    (*head).mask = cap as u64 - 1;
    (*head).has_nulls = 0;
    let mut w = Walk::over(Datum::from_usize(base.add(at) as usize), d.elem, &mut Region::over(&mut []))?;
    while let Some(e) = w.next() {
        if e.isnull {
            (*head).has_nulls |= 1;
            continue;
        }
        if d.user != 0 {
            continue;
        }
        let (tag, value) = match h.hash {
            None => {
                let k = native(e.value.as_u64(), d.lens[1]) as u64;
                (mix(k) as u64 | OCC, k)
            }
            Some((hf, conv)) => match with_region(0, |r| hash(hf, conv[1], d.coll, e.value.as_u64(), r)) {
                Ok(t) => (t as u64 | OCC, e.value.as_u64()),
                Err(_) => {
                    (*head).has_nulls |= HELD;
                    break;
                }
            },
        };
        insert(slots, tag, value, h.hash.is_none());
    }
    Ok(buf)
}

/// PG's hashed answer for operand `v` (non-null) over the set at `set` (the site's word, `bound`'s region):
/// `(found, the array holds a NULL)`; a user key's set and operand classed in run `keys` (the program's `keys.user`).
/// A value's copy (a toasted operand or element under the shims) is charged to `ctx`'s owner (`with_region`; 0: none).
/// # Safety
/// `set` is the address of a region `bound` built for `d`; `keys` is 0 or a `keys.user` handle; `ctx` is 0 or the
/// caller's bound ctx.
// inlined into saop_host.rs `Desc::eval` whichever codegen unit fast-profile gives each file (outlined: a hashed int list 1.14x)
#[inline]
pub(crate) unsafe fn probe(ctx: u64, set: u64, d: &Desc, h: &Hashed, v: u64, keys: u64) -> Result<(bool, bool), Box<PgError>> {
    let head = set as *mut Head;
    let mask = (*head).mask as usize;
    // D2614: the first element hash that raised at bind (`HELD`) raises again here, as PG raises it at its first
    // evaluation; the shims are pure, so the same element raises the same error
    if (*head).has_nulls & HELD != 0 {
        let mut w = Walk::over(Datum::from_usize((head as *const u8).add(16 + (mask + 1) * 16) as usize), d.elem, &mut Region::over(&mut []))?;
        while let Some(e) = w.next() {
            if let (false, Some((hf, conv))) = (e.isnull, h.hash) {
                with_region(ctx, |r| hash(hf, conv[1], d.coll, e.value.as_u64(), r))?;
            }
        }
        return Err(err("an element hash that raised at bind did not raise at the probe"));
    }
    // D2125: a user key's elements classed at the run's first probe (`RUN_AT`), each by its class number in the run
    // (kernels userfp `class_number`: the type's hash proc, then its equality, through fmgr on the leader), the native
    // set's key
    if d.user != 0 {
        let run = gravity_kernels::typesem::userfp::run_serial(keys)?;
        if (*head).has_nulls >> RUN_AT != run {
            let slots = core::slice::from_raw_parts_mut(head.add(1) as *mut [u64; 2], mask + 1);
            slots.fill([0, 0]);
            let mut w = Walk::over(Datum::from_usize((head as *const u8).add(16 + (mask + 1) * 16) as usize), d.elem, &mut Region::over(&mut []))?;
            while let Some(e) = w.next() {
                if !e.isnull {
                    let k = gravity_kernels::typesem::userfp::class_number(keys, e.value.as_usize(), d.user)? as u64;
                    insert(slots, mix(k) as u64 | OCC, k, true);
                }
            }
            (*head).has_nulls = ((*head).has_nulls & ((1 << RUN_AT) - 1)) | run << RUN_AT;
        }
    }
    let slots = core::slice::from_raw_parts(head.add(1) as *const [u64; 2], mask + 1);
    let (tag, key) = match h.hash {
        None => {
            let k = if d.user != 0 { gravity_kernels::typesem::userfp::class_number(keys, v as usize, d.user)? as u64 } else { native(v, d.lens[0]) as u64 };
            (mix(k) as u64 | OCC, Some(k))
        }
        Some((hf, conv)) => (with_region(ctx, |region| hash(hf, conv[0], d.coll, v, region))? as u64 | OCC, None),
    };
    let mut i = tag as usize & mask;
    while slots[i][0] != 0 {
        if slots[i][0] == tag {
            let hit = match key {
                Some(k) => slots[i][1] == k,
                None => match h.eq.as_ref() {
                    Some(eq) => with_region(ctx, |region| call2(eq, d.coll, v, slots[i][1], region))? == Some(true),
                    None => return Err(err("a hashed set with neither its equality nor a native key")),
                },
            };
            if hit {
                return Ok((true, (*head).has_nulls & 1 != 0));
            }
        }
        i = (i + 1) & mask;
    }
    Ok((false, (*head).has_nulls & 1 != 0))
}

/// D4367: the unit tests that take a region through the grant pool hold this while one of them arms the pool to refuse
/// (the pool is the process's).
#[cfg(test)]
pub(crate) static POOL_ARM: std::sync::Mutex<()> = std::sync::Mutex::new(());

#[cfg(test)]
mod tests {
    use super::*;

    /// The tag keeps bit 32 set, so an element whose hash is 0 is never an empty slot.
    #[test]
    fn a_zero_hash_is_occupied() {
        assert_ne!(mix(0) as u64 | OCC, 0);
        assert_ne!(0u64 | OCC, 0);
    }
}
