//! LIST-EXT: expr.saop's extreme form, `x op ANY / ALL` over a constant list under a btree ordering operator (step/expr_saop.rs
//! `extreme_of`, `Hashed::extreme`): x op e holds for some element (ANY) / every element (ALL) exactly when it holds for the
//! one that decides it, the greatest or the least as the operator's family orders them, since that order is total and the
//! operator is its member. At bind the array's image is copied into a grant-pool region the execution holds, its address
//! the site's word (step/raw/saop_set.rs `bound` hands it here), behind a head (the element count, flags, the walk's
//! count, the extreme's datum, which a by-reference extreme points into the image for). The probes walk the list as PG
//! does (execExprInterp.c `ExecEvalScalarArrayOp`) until their walks have visited as many elements as the list holds; the
//! probe that crosses that count finds the extreme (one more walk's compares), and every later probe is one call of the
//! operator. So the form never costs more than twice PG's walk: a qual ahead of it that lets one row through, a list whose
//! first element decides, a handful of rows. Probes of one execution run on pool workers at once: the count and the
//! extreme's publication are atomic. Its own module and out-of-line functions, as the byte set's: trunk's hashed probe
//! stays inlined in saop_host.rs (fast-profile's codegen units). Owner M2-FRAME.
#![allow(unsafe_code)]
use core::ptr::addr_of_mut;
use core::sync::atomic::{AtomicU64, Ordering};

use gravity_expr::nodes::array::Walk;
use gravity_expr::views::image_view;
use gravity_fn::{Datum, PgError, Region};

use crate::step::raw::saop_host::{call2, with_region, Desc, Fn2, NULL};

/// The extreme set's head; the array's image follows it.
#[repr(C)]
struct Head {
    count: u64,
    /// `NULLS` / `HELD` / `VALUE`, written before `READY` is published.
    flags: u64,
    /// The elements the probes' walks have visited; `READY` once the extreme is found.
    walked: AtomicU64,
    /// The extreme's datum (`VALUE`).
    best: u64,
}

/// `Head::flags`: an element is NULL.
const NULLS: u64 = 1;
/// `Head::flags`: a compare of two elements raised (a collation the comparison cannot determine): the probes keep PG's
/// walk, which raises PG's error at its first compare.
const HELD: u64 = 4;
/// `Head::flags`: an element is not NULL (`Head::best` is set).
const VALUE: u64 = 16;
/// `Head::walked`: the extreme is found and `flags` / `best` published.
const READY: u64 = 1 << 63;

const IMAGE_AT: usize = core::mem::size_of::<Head>();

fn err(m: &str) -> Box<PgError> {
    Box::new(PgError::error(format!("gravity: expr.saop: {m}"))) // alloc: Cold
}

/// The extreme set of array `a` (a non-null datum of the node's array type) for `d`, in a region charged to the grant pool
/// (`gravity_driver::in_list_set_region`): its address is the site's word for this execution. No element is compared here.
/// # Safety
/// `a` is a non-null datum of `d`'s array type.
#[inline(never)]
pub(crate) unsafe fn build(d: &Desc, a: u64) -> types_error::PgResult<gravity_driver::RegionBuf> {
    with_region(0, |region| {
        let img = image_view(Datum::from_u64(a), -1, region)?;
        let mut buf = gravity_driver::in_list_set_region(IMAGE_AT + img.len as usize).map_err(|e| super::saop_set::region_err("the list's extreme", e))?;
        let base = buf.as_mut_ptr();
        if base as usize % 8 != 0 {
            return Err(err("the list's extreme region is not 8-aligned"));
        }
        core::ptr::copy_nonoverlapping(img.ptr, base.add(IMAGE_AT), img.len as usize);
        let count = Walk::over(Datum::from_usize(base.add(IMAGE_AT) as usize), d.elem, &mut Region::over(&mut []))?.len() as u64;
        (base as *mut Head).write(Head { count, flags: 0, walked: AtomicU64::new(0), best: 0 });
        Ok(buf)
    })
}

/// PG's linear answer (`FALSE` / `TRUE` / `NULL`) for operand `v` over the extreme set at `set` (the site's word, `build`'s
/// region; NULL for a NULL array): an empty list is false (ANY) / true (ALL) whatever the operand, a NULL operand of the
/// strict operator NULL; before the extreme is found PG's walk; then x op the extreme, NULL where it does not decide and an
/// element is NULL, a list of NULLs NULL.
/// A value's copy under the operator (a toasted operand or element) is charged to `ctx`'s owner (`with_region`; 0: none).
/// # Safety
/// `set` is the address of a region `build` built for `d` unless `a_null`; `ctx` is 0 or the caller's bound ctx.
#[inline(never)]
pub(crate) unsafe fn eval(ctx: u64, d: &Desc, v: u64, v_null: bool, set: u64, a_null: bool) -> Result<u64, Box<PgError>> {
    let b = |t: bool| t as u64;
    if a_null {
        return Ok(NULL);
    }
    let head = set as *mut Head;
    let count = (*head).count;
    if count == 0 {
        return Ok(b(!d.use_or));
    }
    if v_null {
        return Ok(NULL);
    }
    let Some(op) = d.op.as_ref() else { return Err(err("a list's extreme without its operator")) };
    let img = (set as *const u8).add(IMAGE_AT);
    let walked = &*addr_of_mut!((*head).walked);
    if walked.load(Ordering::Acquire) & READY == 0 {
        let (r, seen) = walk(ctx, d, op, v, img)?;
        let before = walked.fetch_add(seen, Ordering::AcqRel);
        // exactly one probe's walk crosses the count
        if before & READY == 0 && before < count && before + seen >= count {
            settle(ctx, d, head);
        }
        return Ok(r);
    }
    let flags = (*head).flags;
    if flags & HELD != 0 {
        return walk(ctx, d, op, v, img).map(|(r, _)| r);
    }
    if flags & VALUE == 0 {
        return Ok(NULL);
    }
    Ok(match with_region(ctx, |region| call2(op, d.coll, v, (*head).best, region))? {
        Some(t) if t == d.use_or => b(t),
        Some(_) if flags & NULLS == 0 => b(!d.use_or),
        _ => NULL,
    })
}

/// The extreme of the set at `head` under the family's `<`, published with `READY`; a compare that raises leaves `HELD`.
#[inline(never)]
unsafe fn settle(ctx: u64, d: &Desc, head: *mut Head) {
    let Some(x) = d.hashed.as_ref().and_then(|h| h.extreme.as_ref()) else { return };
    let img = (head as *const u8).add(IMAGE_AT);
    // one region for the whole walk, as the linear form's (saop_host.rs `Desc::eval`)
    let found = with_region(ctx, |region| {
        let mut w = Walk::over(Datum::from_usize(img as usize), d.elem, &mut Region::over(&mut []))?;
        let (mut flags, mut best) = (0u64, 0u64);
        while let Some(e) = w.next() {
            if e.isnull {
                flags |= NULLS;
                continue;
            }
            let e = e.value.as_u64();
            if flags & VALUE == 0 {
                (best, flags) = (e, flags | VALUE);
                continue;
            }
            // the greatest: the extreme so far below `e`; the least: `e` below it
            let (l, r) = if x.greatest { (best, e) } else { (e, best) };
            if call2(&x.less, d.coll, l, r, region)? == Some(true) {
                best = e;
            }
        }
        Ok((flags, best))
    });
    let (flags, best) = found.unwrap_or((HELD, 0));
    addr_of_mut!((*head).best).write(best);
    addr_of_mut!((*head).flags).write(flags);
    (*addr_of_mut!((*head).walked)).fetch_or(READY, Ordering::Release);
}

/// PG's walk: the operator over each element in order (its first compare raises its error); the answer and the elements
/// visited.
#[inline(never)]
unsafe fn walk(ctx: u64, d: &Desc, op: &Fn2, v: u64, img: *const u8) -> Result<(u64, u64), Box<PgError>> {
    with_region(ctx, |region| {
        let mut w = Walk::over(Datum::from_usize(img as usize), d.elem, &mut Region::over(&mut []))?;
        let (mut null, mut seen) = (false, 0u64);
        while let Some(e) = w.next() {
            seen += 1;
            if e.isnull {
                null = true;
                continue;
            }
            match call2(op, d.coll, v, e.value.as_u64(), region)? {
                None => null = true,
                Some(t) if t == d.use_or => return Ok((t as u64, seen)),
                Some(_) => {}
            }
        }
        Ok((if null { NULL } else { !d.use_or as u64 }, seen))
    })
}

#[cfg(test)]
mod tests {
    use gravity_expr::nodes::array::ElemFacts;

    use super::*;
    use crate::step::raw::saop_host::{fn2, Extreme, Hashed};

    /// A one-dimensional array image of type `typ` (element `len` bytes by value, or a 4-byte-header varlena for `len` -1,
    /// int-aligned), `None` elements NULL with the bitmap, in 8-byte words.
    fn array(typ: u32, len: i16, elems: &[Option<Vec<u8>>]) -> Vec<u64> {
        let nulls = elems.iter().any(|e| e.is_none());
        let bitmap = if nulls { elems.len().div_ceil(8) } else { 0 };
        let mut data = Vec::new(); // alloc: Cold
        for e in elems.iter().flatten() {
            if len < 0 {
                data.extend_from_slice(&(((4 + e.len()) as u32) << 2).to_le_bytes());
            }
            data.extend_from_slice(e);
            data.resize(data.len().div_ceil(len.max(4) as usize) * len.max(4) as usize, 0);
        }
        let head = (24 + bitmap).div_ceil(8) * 8;
        let mut img = vec![0u8; head]; // alloc: Cold
        img[4..8].copy_from_slice(&1i32.to_le_bytes());
        img[8..12].copy_from_slice(&(if nulls { head as i32 } else { 0 }).to_le_bytes());
        img[12..16].copy_from_slice(&typ.to_le_bytes());
        img[16..20].copy_from_slice(&(elems.len() as i32).to_le_bytes());
        img[20..24].copy_from_slice(&1i32.to_le_bytes());
        for (i, e) in elems.iter().enumerate() {
            if e.is_some() && nulls {
                img[24 + i / 8] |= 1 << (i % 8);
            }
        }
        img.extend_from_slice(&data);
        let n = img.len() as u32;
        img[0..4].copy_from_slice(&(n << 2).to_le_bytes());
        let mut w = vec![0u64; img.len().div_ceil(8)]; // alloc: Cold
        unsafe { core::ptr::copy_nonoverlapping(img.as_ptr(), w.as_mut_ptr().cast::<u8>(), img.len()) };
        w
    }

    /// A sealed extreme Desc: operator `op` under ANY (`use_or`) or ALL, the family's `<` `less`, `greatest` as
    /// typesem `list_order` reads `op` (`<` / `<=`: the greatest decides ANY).
    fn desc(op: u32, lt_family: bool, use_or: bool, less: u32, elem: ElemFacts, coll: u32) -> Desc {
        let s = |f: u32| gravity_fn::shim_of(f).unwrap_or_else(|| panic!("no shim {f}"));
        let lens = [elem.len, elem.len];
        let extreme = Some(Extreme { less: fn2(s(less), lens).unwrap(), greatest: lt_family == use_or });
        let hashed = Some(Hashed { eq: None, hash: None, bytes: false, images: false, extreme, trim: false });
        Desc { use_or, coll, op: fn2(s(op), lens), elem, lens, hashed, user: 0, image_eq: 0, fp: 0 }.sealed()
    }

    /// PG's walk (ExecEvalScalarArrayOp) through the same operator: the reference answer, or whether it raised.
    fn walked(d: &Desc, v: Option<u64>, a: &[u64]) -> Result<u64, ()> {
        let op = d.op.as_ref().unwrap();
        let mut w = Walk::over(Datum::from_usize(a.as_ptr() as usize), d.elem, &mut Region::over(&mut [])).unwrap();
        if w.is_empty() {
            return Ok(!d.use_or as u64);
        }
        let Some(v) = v else { return Ok(NULL) };
        let mut null = false;
        while let Some(e) = w.next() {
            let r = if e.isnull { None } else { unsafe { with_region(0, |r| call2(op, d.coll, v, e.value.as_u64(), r)) }.map_err(|_| ())? };
            match r {
                None => null = true,
                Some(t) if t == d.use_or => return Ok(t as u64),
                Some(_) => {}
            }
        }
        Ok(if null { NULL } else { !d.use_or as u64 })
    }

    /// The extreme set's answer for each operand against the walk's, over every list, both senses and the four operators
    /// of a family (`ops`: `<`, `<=`, `>=`, `>`; `less` its `<`).
    fn agree(name: &str, ops: [u32; 4], less: u32, elem: ElemFacts, coll: u32, lists: &[Vec<Option<Vec<u8>>>], operands: &[Option<u64>]) {
        let _pool = crate::step::raw::saop_set::POOL_ARM.lock().unwrap_or_else(|e| e.into_inner());
        for (k, &op) in ops.iter().enumerate() {
            for use_or in [true, false] {
                let d = desc(op, k < 2, use_or, less, elem, coll);
                for l in lists {
                    let img = array(elem.typ, elem.len, l);
                    let set = unsafe { build(&d, img.as_ptr() as u64) }.unwrap();
                    let p = set.as_ptr() as u64;
                    // the walk until a list's worth of elements, then (settled here if the probes did not get there) the extreme
                    for round in 0..2 {
                        for &v in operands {
                            let want = walked(&d, v, &img);
                            let got = unsafe { eval(0, &d, v.unwrap_or(0), v.is_none(), p, false) }.map_err(|_| ());
                            assert_eq!(got, want, "{name} op {op} {} list {l:?} operand {v:?} round {round}", if use_or { "ANY" } else { "ALL" });
                        }
                        if !ready(p) {
                            unsafe { settle(0, &d, p as *mut Head) };
                        }
                    }
                    let d2 = unsafe { eval(0, &d, v_or(operands), false, 0, true) }.unwrap();
                    assert_eq!(d2, NULL, "a NULL array");
                }
            }
        }
    }

    fn ready(p: u64) -> bool {
        unsafe { (*(p as *const Head)).walked.load(Ordering::Acquire) & READY != 0 }
    }

    fn v_or(operands: &[Option<u64>]) -> u64 {
        operands.iter().flatten().next().copied().unwrap_or(0)
    }

    fn lists_of(vals: &[u64], len: usize) -> Vec<Vec<Option<Vec<u8>>>> {
        let le = |v: u64| v.to_le_bytes()[..len].to_vec();
        let mut out: Vec<Vec<Option<Vec<u8>>>> = vec![vec![], vec![None], vec![None, None]]; // alloc: Cold
        for (i, &v) in vals.iter().enumerate() {
            out.push(vec![Some(le(v))]);
            out.push(vals.iter().cycle().skip(i).take(i % 5 + 2).map(|&x| Some(le(x))).collect());
            let mut withnull: Vec<Option<Vec<u8>>> = vals.iter().cycle().skip(i * 3).take(i % 7 + 1).map(|&x| Some(le(x))).collect(); // alloc: Cold
            withnull.insert(i % (withnull.len() + 1), None);
            out.push(withnull);
        }
        out.push(vals.iter().map(|&x| Some(le(x))).collect());
        out
    }

    /// int4 `<` `<=` `>=` `>` (int4lt / int4le / int4ge / int4gt; the family's `<` int4lt): ties, the extremes of the type,
    /// NULL elements deciding and not, the empty list, a NULL operand.
    #[test]
    fn an_int_list_answers_as_its_walk() {
        let int4 = ElemFacts { typ: 23, len: 4, byval: true, align: b'i' as i8 };
        let vals: Vec<u64> = [3i32, -2, 7, 7, 0, i32::MIN, i32::MAX, -5, 2].iter().map(|&x| x as u32 as u64).collect(); // alloc: Cold
        let operands: Vec<Option<u64>> = [None].into_iter().chain([-6i32, -5, -2, 0, 2, 3, 6, 7, 8, i32::MIN, i32::MAX].map(|x| Some(x as u32 as u64))).collect(); // alloc: Cold
        agree("int4", [66, 149, 150, 147], 66, int4, 0, &lists_of(&vals, 4), &operands);
    }

    /// float8 (float8lt / le / ge / gt; the family's `<` float8lt): NaN above every number and equal to itself, -0 = 0,
    /// the infinities.
    #[test]
    fn a_float_list_orders_nan_as_its_family_does() {
        let float8 = ElemFacts { typ: 701, len: 8, byval: true, align: b'd' as i8 };
        let vals: Vec<u64> = [1.5f64, f64::NAN, -0.0, 0.0, f64::INFINITY, f64::NEG_INFINITY, -3.25, f64::NAN].iter().map(|x| x.to_bits()).collect(); // alloc: Cold
        let operands: Vec<Option<u64>> = [None].into_iter().chain([f64::NAN, 0.0, -0.0, 1.5, 2.0, f64::INFINITY, f64::NEG_INFINITY, -4.0].map(|x| Some(x.to_bits()))).collect(); // alloc: Cold
        agree("float8", [295, 296, 298, 297], 295, float8, 0, &lists_of(&vals, 8), &operands);
    }

    /// text under "C" (text_lt / text_le / text_ge / text_gt; the family's `<` text_lt): the empty string, prefixes, case.
    #[test]
    fn a_text_list_orders_by_its_collation() {
        let text = ElemFacts { typ: 25, len: -1, byval: false, align: b'i' as i8 };
        let words: [&[u8]; 7] = [b"b", b"", b"ab", b"B", b"abc", b"b", "é".as_bytes()];
        let lists: Vec<Vec<Option<Vec<u8>>>> = (0..words.len()).flat_map(|i| {
            let run: Vec<Option<Vec<u8>>> = words.iter().cycle().skip(i).take(i + 1).map(|w| Some(w.to_vec())).collect(); // alloc: Cold
            let mut nul = run.clone();
            nul.insert(i % (run.len() + 1), None);
            [run, nul]
        }).chain([vec![], vec![None]]).collect(); // alloc: Cold
        let ops: Vec<Vec<u64>> = words.iter().chain([&b"a"[..], b"c", b"abd"].iter()).map(|w| { // alloc: Cold
            let mut img = (((4 + w.len()) as u32) << 2).to_le_bytes().to_vec(); // alloc: Cold
            img.extend_from_slice(w);
            let mut x = vec![0u64; img.len().div_ceil(8)]; // alloc: Cold
            unsafe { core::ptr::copy_nonoverlapping(img.as_ptr(), x.as_mut_ptr().cast::<u8>(), img.len()) };
            x
        }).collect();
        let operands: Vec<Option<u64>> = [None].into_iter().chain(ops.iter().map(|x| Some(x.as_ptr() as u64))).collect(); // alloc: Cold
        agree("text C", [740, 741, 743, 742], 740, text, 950, &lists, &operands);
    }

    /// A compare that raises (text with no collation): the probe raises where PG's walk raises, before and after the
    /// extreme is sought, and answers where it answers (an empty list, a NULL operand).
    #[test]
    fn a_compare_that_raises_raises_at_the_probe() {
        let text = ElemFacts { typ: 25, len: -1, byval: false, align: b'i' as i8 };
        let lists: Vec<Vec<Option<Vec<u8>>>> = vec![vec![Some(b"a".to_vec()), Some(b"b".to_vec())], vec![None, Some(b"x".to_vec()), Some(b"y".to_vec())], vec![]]; // alloc: Cold
        let v = (((4 + 1) as u32) << 2) as u64 | (b'c' as u64) << 32;
        agree("text no collation", [740, 741, 743, 742], 740, text, 0, &lists, &[None, Some(&v as *const u64 as u64)]);
    }

    fn int4_list(vals: &[i32]) -> Vec<u64> {
        array(23, 4, &vals.iter().map(|&x| Some((x as u32).to_le_bytes().to_vec())).collect::<Vec<_>>()) // alloc: Cold
    }

    /// The probes walk until their walks have visited a list's worth of elements; the probe that crosses it finds the
    /// extreme, and the later probes answer from it: a first element that decides takes ten probes of a ten-element list,
    /// an operand no element decides one.
    #[test]
    fn the_probes_walk_a_lists_worth_then_answer_from_the_extreme() {
        let _pool = crate::step::raw::saop_set::POOL_ARM.lock().unwrap_or_else(|e| e.into_inner());
        let int4 = ElemFacts { typ: 23, len: 4, byval: true, align: b'i' as i8 };
        let d = desc(66, true, true, 66, int4, 0);
        let img = int4_list(&[1, 2, 3, 4, 5, 6, 7, 8, 9, 10]);
        let set = unsafe { build(&d, img.as_ptr() as u64) }.unwrap();
        let p = set.as_ptr() as u64;
        for k in 1..=10 {
            assert!(!ready(p), "settled after {} probes", k - 1);
            assert_eq!(unsafe { eval(0, &d, 0, false, p, false) }.unwrap(), 1);
        }
        assert!(ready(p));
        assert_eq!(unsafe { (*(p as *const Head)).best }, 10);
        for (v, want) in [(9i32, 1u64), (10, 0), (11, 0), (i32::MIN, 1)] {
            assert_eq!(unsafe { eval(0, &d, v as u32 as u64, false, p, false) }.unwrap(), want, "{v} < ANY (1 .. 10)");
        }
        let set = unsafe { build(&d, img.as_ptr() as u64) }.unwrap();
        let p = set.as_ptr() as u64;
        assert_eq!(unsafe { eval(0, &d, 100, false, p, false) }.unwrap(), 0);
        assert!(ready(p), "a walk of the whole list settles it");
    }

    /// Pool workers probe one execution's set at once: every answer is the walk's, the extreme found once.
    #[test]
    fn workers_probing_at_once_answer_as_the_walk() {
        let _pool = crate::step::raw::saop_set::POOL_ARM.lock().unwrap_or_else(|e| e.into_inner());
        struct Shared<'a>(&'a Desc, u64);
        // SAFETY: the set's shared state is atomic; the Desc is read only.
        unsafe impl Sync for Shared<'_> {}
        let int4 = ElemFacts { typ: 23, len: 4, byval: true, align: b'i' as i8 };
        let d = desc(149, true, true, 66, int4, 0);
        let vals: Vec<i32> = (0..64).map(|k| (k * 37) % 101 - 50).collect(); // alloc: Cold
        let img = int4_list(&vals);
        let set = unsafe { build(&d, img.as_ptr() as u64) }.unwrap();
        let s = Shared(&d, set.as_ptr() as u64);
        let max = *vals.iter().max().unwrap();
        std::thread::scope(|sc| {
            for t in 0..4 {
                let s = &s;
                sc.spawn(move || {
                    for k in 0..2000i32 {
                        let v = (k * 7 + t * 13) % 160 - 80;
                        let got = unsafe { eval(0, s.0, v as u32 as u64, false, s.1, false) }.unwrap();
                        assert_eq!(got, (v <= max) as u64, "{v} <= ANY, thread {t}");
                    }
                });
            }
        });
        assert!(ready(s.1));
    }
}
