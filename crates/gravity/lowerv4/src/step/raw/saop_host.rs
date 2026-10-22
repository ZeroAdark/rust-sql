//! expr.saop's host half (step/expr_saop.rs claims the node): the claimed node's `Desc`, owned by the program, and
//! `saop_eval`, PG's hashed and linear ScalarArrayOp over it (execExprInterp.c `ExecEvalHashedScalarArrayOp` /
//! `ExecEvalScalarArrayOp`) through the functions' shims. Owner M2-FRAME.
#![allow(unsafe_code)]
use gravity_expr::nodes::array::{ElemFacts, Walk};
use gravity_expr::nodes::func::ArgConv;
use gravity_expr::views::{image_view, no_space, payload_view};
use gravity_fn::{Arg, ArgKind, Datum, ErrSlot, PgError, Region, Shim};

/// Equality `eq` over by-value operands of lengths `lens` answers as their sign-extended values compare (typesem
/// `native_eq` over 1-, 2-, 4- or 8-byte words): a hashed set keys by that value, and the linear form compares it without
/// the shim.
pub(crate) fn native_keyed(eq: u32, byval: bool, lens: [i16; 2]) -> bool {
    gravity_fn::typesem::expr::native_eq(eq) && byval && lens.iter().all(|&l| matches!(l, 1 | 2 | 4 | 8))
}

/// `saop_eval`'s answers besides false (0).
pub(crate) const TRUE: u64 = 1;
pub(crate) const NULL: u64 = 2;
pub(crate) const FAILED: u64 = 3;

/// A two-argument shim with its arguments' forms.
pub(crate) struct Fn2 {
    pub(crate) shim: &'static Shim,
    pub(crate) conv: [ArgConv; 2],
}

/// The hashed form of a constant array PG's plan hashed (`hashfuncid`): literal-free, its set built once per execution at
/// bind and bound as the site's word (step/raw/saop_set.rs, V4R-122).
pub(crate) struct Hashed {
    /// The equality the set compares with (the node's operator under IN, its `negfuncid` under NOT IN); `None` for a
    /// user key's set (`Desc::user`), which compares class numbers.
    pub(crate) eq: Option<Fn2>,
    /// The hash function over the operand (`[0]`) and an element (`[1]`), or `None` for a native (integer) set.
    pub(crate) hash: Option<(&'static Shim, [ArgConv; 2])>,
    /// LIST-FORM (CAP-10 / STEP-14): a byte set, the text equality under a collation whose equality is byte equality
    /// (typesem `coll_bytes`): keyed by the payload's length and a hash of its head and tail bytes, compared byte for byte
    /// (step/raw/saop_bytes.rs `bytes_tag`); `eq` and `hash` are then `None`. Any constant list of at least
    /// `cost::forms::text_set_min()` elements, PG's hashed one or not (PG's answer for these is the bytes' equality either way).
    pub(crate) bytes: bool,
    /// Road text.set (rep/varlena/text_set.rs): a byte set whose site probes it in line, so its bind lays out the
    /// elements' images too (step/raw/saop_bytes.rs `images`).
    pub(crate) images: bool,
    /// LIST-EXT: an ordering operator's constant list as its extreme element (step/raw/saop_extreme.rs); `eq` and `hash`
    /// are then `None`.
    pub(crate) extreme: Option<Extreme>,
    /// D4794 (road text.set.bpchar, default off): a byte set over bpchar's `=` / `<>` (bpchareq under ANY, bpcharne under
    /// ALL): every element and every operand keyed and compared without its trailing blanks, as bpchareq's bcTruelen
    /// drops them on both sides (step/raw/saop_bytes.rs `TRIM`).
    pub(crate) trim: bool,
}

/// LIST-EXT: `x op ANY / ALL` over a constant list under a btree ordering operator (typesem `list_order`) is `x op` the
/// one element that decides it: the greatest as the operator's family orders the elements (`less`) under ANY of `<` /
/// `<=` and ALL of `>=` / `>`, else the least, found once per execution by the probe whose walk crosses the list's
/// element count (step/raw/saop_extreme.rs).
pub(crate) struct Extreme {
    /// The family's `<` over two elements (its collation, its NaN).
    pub(crate) less: Fn2,
    pub(crate) greatest: bool,
}

/// One claimed node, owned by the program (its index in the owned objects is an IR constant, V4R-101).
pub(crate) struct Desc {
    pub(crate) use_or: bool,
    pub(crate) coll: u32,
    /// The node's operator, for the linear form (`None`: a user key's hashed site, which never runs it).
    pub(crate) op: Option<Fn2>,
    pub(crate) elem: ElemFacts,
    /// The operand's and the element's lengths (a native set's normalisation).
    pub(crate) lens: [i16; 2],
    pub(crate) hashed: Option<Hashed>,
    /// D2125: the user key (typesem `user_key` spec, 0 for none) of a hashed site whose equality and hash function are
    /// the operand type's user support functions (a user DEFAULT hash class): its set holds the elements' class numbers
    /// (kernels userfp `class_number`: the hash proc through fmgr, then the equality against the bucket's
    /// representatives, as PG's hashed ScalarArrayOp calls them), and a row's operand is classed the same way, on the
    /// leader. `hashed.hash` is then `None` (the class number is the native key).
    pub(crate) user: u64,
    /// The bytes a linear walk compares in place of the operator (0: the operator runs): the element type's `typlen`
    /// when the node's operator is that fixed-length by-reference type's own `=` and its equal values are equal stored
    /// bytes (typesem `bytes_leaf`: uuid_eq is `memcmp(.., UUID_LEN) == 0`, uuid.c `uuid_internal_cmp`); minus it when
    /// the operator is that `=`'s negator (`<>`), whose answer is the compare's negation.
    pub(crate) image_eq: i16,
    /// D2248: `sealed`'s fold of the fields above, the site's IR constant beside the index, checked by `saop_eval`.
    pub(crate) fp: u64,
}

// SAFETY: a Desc is immutable after the lowering; the shims are pure functions over their arguments.
unsafe impl Sync for Desc {}
unsafe impl Send for Desc {}

pub(crate) fn fn2(shim: &'static Shim, lens: [i16; 2]) -> Option<Fn2> {
    if shim.nargs != 2 || !shim.tyargs.is_empty() || shim.region {
        return None;
    }
    let conv = |i: usize| ArgConv { kind: shim.args[i], typlen: if matches!(shim.args[i], ArgKind::V | ArgKind::D) { 0 } else { lens[i] } };
    Some(Fn2 { shim, conv: [conv(0), conv(1)] })
}

/// `f` over the views' region (a short-header copy, a detoasted operand or array): 8 KiB of this frame's stack, then a heap
/// buffer grown fourfold (to 1 GiB) while `f` fails for want of space (a large toasted datum, which PG pallocs per row too).
/// A row's call (`saop_eval`'s) passes its `ctx`: the heap half is one value's buffer (CLASS E), charged to `ctx`'s owner
/// for its life (rep/varlena `value`); a grow makes the new buffer before the old one (and its charge) goes. `ctx` 0, no
/// owner: the bind road (a hashed / extreme set's build, before any pipeline runs) and the tests.
pub(crate) fn with_region<T>(ctx: u64, mut f: impl FnMut(&mut Region) -> Result<T, Box<PgError>>) -> Result<T, Box<PgError>> {
    let mut stack = [core::mem::MaybeUninit::<u8>::uninit(); 8 << 10];
    let (mut heap, s) = (gravity_fn::arena::ChargedVec::<u8>::none(), stack.as_mut_ptr_range());
    let mut region = Region { cur: gravity_abi::word(s.start.cast()), end: gravity_abi::word(s.end.cast()) };
    loop {
        match f(&mut region) {
            Err(e) if gravity_fn::fmgr_call::FmgrCall::error_of(&e).0 == gravity_fn::fmgr_call::FmgrCall::error_of(&no_space()).0 && heap.capacity() < 1 << 30 => {
                let cap = heap.capacity().max(8 << 10) * 4;
                let mut grown = crate::rep::varlena::raw::value::vec(ctx, cap);
                grown.with_vec(|v| v.reserve_exact(cap)); // alloc: Cold charged value::owner(ctx, cap)
                heap = grown;
                let h = heap.with_vec(|v| v.spare_capacity_mut().as_mut_ptr_range());
                region = Region { cur: gravity_abi::word(h.start.cast()), end: gravity_abi::word(h.end.cast()) };
            }
            r => return r,
        }
    }
}

/// `arg` of kind `conv` as a shim argument.
unsafe fn arg(conv: ArgConv, d: u64, region: &mut Region) -> Result<Arg, Box<PgError>> {
    Ok(match conv.kind {
        ArgKind::V | ArgKind::D => Arg::V(d as i64),
        ArgKind::P => Arg::R(payload_view(Datum::from_u64(d), conv.typlen, region)?),
        ArgKind::I => Arg::R(image_view(Datum::from_u64(d), conv.typlen, region)?),
    })
}

/// `f(a, b)` under collation `coll`: `Some(bool)`, `None` for NULL.
/// The region is rewound after the call (the views and the bool result outlive nothing).
pub(crate) unsafe fn call2(f: &Fn2, coll: u32, a: u64, b: u64, region: &mut Region) -> Result<Option<bool>, Box<PgError>> {
    let mark = region.cur;
    let args = [arg(f.conv[0], a, region)?, arg(f.conv[1], b, region)?];
    let mut e = ErrSlot::NONE;
    let r = (f.shim.call)(&mut e, region, coll, &[], &args);
    region.cur = mark;
    match r.status {
        gravity_fn::OK => Ok(Some(r.val != 0)),
        gravity_fn::NULL => Ok(None),
        gravity_fn::ERROR => Err(e.take().unwrap_or_else(no_space)),
        _ => Err(no_space()),
    }
}

/// The hash of `d` (conv `conv`) under `coll`; a NULL result (a strict hash never gives one) hashes as 0.
pub(crate) unsafe fn hash(h: &Shim, conv: ArgConv, coll: u32, d: u64, region: &mut Region) -> Result<u32, Box<PgError>> {
    let mark = region.cur;
    let a = [arg(conv, d, region)?];
    let mut e = ErrSlot::NONE;
    let r = (h.call)(&mut e, region, coll, &[], &a);
    region.cur = mark;
    match r.status {
        gravity_fn::OK => Ok(r.val as u32),
        gravity_fn::NULL => Ok(0),
        gravity_fn::ERROR => Err(e.take().unwrap_or_else(no_space)),
        _ => Err(no_space()),
    }
}

/// A by-value datum of length `len` sign-extended (a "char" / bool byte zero-extended), as a native set keys it.
pub(crate) fn native(d: u64, len: i16) -> i64 {
    match len {
        1 => (d as u8) as i64,
        2 => (d as i16) as i64,
        4 => (d as i32) as i64,
        _ => d as i64,
    }
}

impl Desc {
    /// D2248: `self` with its fingerprint, a fold (FNV-1a) of every fact `eval` and the set read. In the site's IR it keys
    /// a program whose Desc answers otherwise apart (the driver reuses a parked Node by the program's key, V4R-101 keeps
    /// the Desc out of the IR).
    pub(crate) fn sealed(mut self) -> Self {
        let c = |x: ArgConv| ((x.kind as u64) << 16) | x.typlen as u16 as u64;
        let f = |s: &Shim, v: [ArgConv; 2]| [s.oid as u64, c(v[0]), c(v[1])];
        let (e, h) = (self.elem, self.hashed.as_ref());
        let (eq, hf) = (h.and_then(|h| h.eq.as_ref()).map_or([0; 3], |e| f(e.shim, e.conv)), h.and_then(|h| h.hash).map_or([0; 3], |(s, v)| f(s, v)));
        let facts = [self.use_or as u64, self.coll as u64, h.map_or(0, |h| 1 + h.hash.is_some() as u64), e.typ as u64, e.len as u16 as u64, e.byval as u64, e.align as u8 as u64, self.lens[0] as u16 as u64, self.lens[1] as u16 as u64];
        // a user key's spec folds in last and only when set, so every other site's constant (its IR pin) stays as it was
        let user = (self.user != 0).then_some(self.user);
        let image = (self.image_eq != 0).then_some(self.image_eq as u64);
        // a byte set folds in last and only when set (every other site's constant stays as it was)
        let bytes = h.is_some_and(|h| h.bytes).then_some(0xb7e5);
        // road text.set: a byte set laid out with its images folds in last and only when set
        let images = h.is_some_and(|h| h.images).then_some(0x1ae5);
        // LIST-EXT: the extreme's order folds in last and only when set
        let extreme = h.and_then(|h| h.extreme.as_ref()).map(|x| f(x.less.shim, x.less.conv).into_iter().chain([0xe7e5, x.greatest as u64])).into_iter().flatten();
        // D4794: a trimmed (bpchar) byte set folds in last and only when set
        let trim = h.is_some_and(|h| h.trim).then_some(0x7e1b);
        let w = facts.into_iter().chain(self.op.as_ref().map_or([0; 3], |o| f(o.shim, o.conv))).chain(eq).chain(hf).chain(user).chain(image).chain(bytes).chain(images).chain(extreme).chain(trim);
        self.fp = w.flat_map(u64::to_le_bytes).fold(0xcbf2_9ce4_8422_2325u64, |a, b| (a ^ b as u64).wrapping_mul(0x0100_0000_01b3));
        self
    }

    /// PG's answer for operand `v` over array `a` (`FALSE` / `TRUE` / `NULL`); a hashed site's `a` is its set's word, a
    /// user key's (`user`) classed in run `keys` (the program's `keys.user` handle, D4197; 0 at every other site). A value's
    /// copy is charged to `ctx`'s owner (`with_region`; 0: none).
    unsafe fn eval(&self, ctx: u64, v: u64, v_null: bool, a: u64, a_null: bool, keys: u64) -> Result<u64, Box<PgError>> {
        let b = |x: bool| x as u64;
        if let Some(h) = &self.hashed {
            // LIST-EXT: the linear form's answer from the list's extreme (its own function, as the byte set's)
            if h.extreme.is_some() {
                return crate::step::raw::saop_extreme::eval(ctx, self, v, v_null, a, a_null);
            }
            // ExecEvalHashedScalarArrayOp: the operator is strict, the array a non-empty constant (its set a word here)
            if v_null || a_null {
                return Ok(NULL);
            }
            // LIST-FORM: a byte set probes by its bytes (its own function: the trunk sets' probe stays as it was inlined)
            let (found, has_nulls) = if h.bytes { crate::step::raw::saop_bytes::probe_bytes(ctx, a, v)? } else { crate::step::raw::saop_set::probe(ctx, a, self, h, v, keys)? };
            if !found && has_nulls {
                return Ok(NULL);
            }
            return Ok(b(if self.use_or { found } else { !found }));
        }
        // ExecEvalScalarArrayOp: the array a word, a constant's too (V4R-110)
        if a_null {
            return Ok(NULL);
        }
        let Some(op) = self.op.as_ref() else { return Err(Box::new(PgError::error("gravity: expr.saop: a user key's site without its set"))) }; // alloc: Cold
        let arr = Datum::from_u64(a);
        let nv = native_keyed(op.shim.oid, self.elem.byval, self.lens).then(|| native(v, self.lens[0]));
        let (img, ne) = (self.image_eq.unsigned_abs() as usize, self.image_eq < 0);
        with_region(ctx, |region| {
            let mut w = Walk::over(arr, self.elem, region)?;
            if w.is_empty() {
                return Ok(b(!self.use_or));
            }
            if v_null {
                return Ok(NULL);
            }
            let mut null = false;
            while let Some(e) = w.next() {
                let r = match nv {
                    _ if e.isnull => None,
                    // the equality is the values' (typesem native_eq): strict, never NULL nor raising over two values
                    Some(n) => Some(n == native(e.value.as_u64(), self.lens[1])),
                    // two fixed-length images in place (the operand's datum and the element in the array's data)
                    None if img > 0 => Some((core::slice::from_raw_parts(v as usize as *const u8, img) == core::slice::from_raw_parts(e.value.as_u64() as usize as *const u8, img)) != ne),
                    None => call2(op, self.coll, v, e.value.as_u64(), region)?,
                };
                match r {
                    None => null = true,
                    Some(x) if x == self.use_or => return Ok(b(x)),
                    Some(_) => {}
                }
            }
            Ok(if null { NULL } else { b(!self.use_or) })
        })
    }
}

/// `Px::owned[i]` of the running program from its `ProgramBlock::owner` (V4R-101): frame/program.rs `emit` keeps the pair
/// `(Px::data, Px::owned)`.
/// # Safety
/// `owner` is the running program's `ProgramBlock::owner`.
unsafe fn owned_at<'a>(owner: *const u8, i: usize) -> Option<&'a dyn core::any::Any> {
    let kept = crate::hir::kept(owner)?.downcast_ref::<(Vec<Box<[u8]>>, Vec<Box<dyn core::any::Any>>)>()?;
    kept.1.get(i).map(|b| &**b)
}

/// The claimed node's `Desc` at `Px::owned[i]` (`owned_at`).
/// # Safety
/// As `owned_at`.
pub(crate) unsafe fn owned_desc<'a>(owner: *const u8, i: usize) -> Option<&'a Desc> {
    owned_at(owner, i)?.downcast_ref::<std::sync::Arc<Desc>>().map(|d| &**d)
}

/// D2248: the Desc when its fingerprint is `fp` (`saop_eval`'s last argument, the site's), else an internal error: never
/// another statement's answer (fail closed).
fn site(d: Option<&Desc>, fp: u64) -> Result<&Desc, &'static str> {
    d.ok_or("the program owns no such site").and_then(|d| (d.fp == fp).then_some(d).ok_or("the site's Desc is another program's"))
}

gravity_kernels::contract::host! {
    /// The claimed node's answer (`desc` its `Desc`'s index in the program's owned objects, reached through `ctx`'s program
    /// block's `ProgramBlock::owner`; `a` the array's word, a hashed site's set; `nulls` bit 0 the operand's NULL, bit 1 the
    /// array's): `FALSE` / `TRUE` / `NULL`, or `FAILED` with the error stashed for the entry (the row's raise block takes
    /// it, as a hosted call's). A value's copy is charged to `ctx`'s owner (CLASS E, `with_region`).
    /// # Safety
    /// `ctx` is the caller's bound ctx (its program block the running program's); `v` / `a` are datums of the operand's /
    /// array's types unless NULL (a hashed site's `a`: its set, `bound`'s region for this execution).
    pub(crate) unsafe fn saop_eval(ctx: u64, desc: u64, v: u64, a: u64, nulls: u64, fp: u64) -> u64 {
        evaluated(ctx, desc, v, a, nulls, fp, 0)
    }

    /// D4197: `saop_eval` at a user key's hashed site (`Desc::user`), its set and operand classed in run `keys` (the
    /// program's `keys.user` handle: kernels userfp `class_number`).
    /// # Safety
    /// As `saop_eval`; `keys` is the program's `keys.user` word.
    pub(crate) unsafe fn saop_eval_keyed(ctx: u64, desc: u64, v: u64, a: u64, nulls: u64, fp: u64, keys: u64) -> u64 {
        evaluated(ctx, desc, v, a, nulls, fp, keys)
    }
}

/// `saop_eval`'s body (`keys`: `saop_eval_keyed`'s run, else 0).
/// # Safety
/// As `saop_eval`.
unsafe fn evaluated(ctx: u64, desc: u64, v: u64, a: u64, nulls: u64, fp: u64, keys: u64) -> u64 {
    // the host ABI takes every pointer as a 64-bit word (gravity_abi::host; wasm32: the word, not an i32); the program's
    // owner (V4R-101: the Desc through it, no per-lowering address in the IR) is the ctx's program block's
    let owner = gravity_abi::unword((*gravity_abi::unword((*(ctx as usize as *const gravity_abi::ctx::Ctx)).program)).owner) as *const u8;
    let r = std::panic::catch_unwind(core::panic::AssertUnwindSafe(|| unsafe { // unwind-ok: c-callback
        let d = site(owned_desc(owner, desc as usize), fp).map_err(|m| Box::new(PgError::error(format!("gravity: expr.saop: {m}")))); // alloc: Cold
        d.and_then(|d| d.eval(ctx, v, nulls & 1 != 0, a, nulls & 2 != 0, keys))
    }));
    match r {
        Ok(Ok(r)) => r,
        Ok(Err(e)) => {
            gravity_fn::stash_error(e);
            FAILED
        }
        Err(payload) => {
            gravity_abi::status::unwind::park(payload);
            FAILED
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The native set's key: each length sign-extended, so an int4 operand meets its int8 element (int48eq).
    #[test]
    fn native_keys_compare_across_lengths() {
        assert_eq!(native((-5i32) as u32 as u64, 4), native((-5i64) as u64, 8));
        assert_eq!(native((-5i16) as u16 as u64, 2), -5);
        assert_eq!(native(0xFF, 1), 255);
    }

    const INT4: ElemFacts = ElemFacts { typ: 23, len: 4, byval: true, align: b'i' as i8 };

    /// A sealed Desc over int4 operator `op` (`hashed`: 0 linear, 1 a native set, 2 a set keyed by hashint4).
    fn desc(use_or: bool, op: u32, coll: u32, elem: ElemFacts, hashed: u8) -> Desc {
        let s = |f: u32| gravity_fn::shim_of(f).unwrap_or_else(|| panic!("no shim {f}"));
        let lens = [4, elem.len];
        let eq = || fn2(s(65), lens).unwrap();
        let hashed = match hashed {
            0 => None,
            1 => Some(Hashed { eq: Some(eq()), hash: None, bytes: false, images: false, extreme: None, trim: false }),
            _ => Some(Hashed { eq: Some(eq()), hash: Some((s(450), [ArgConv { kind: ArgKind::V, typlen: 0 }; 2])), bytes: false, images: false, extreme: None, trim: false }),
        };
        Desc { use_or, coll, op: fn2(s(op), lens), elem, lens, hashed, user: 0, image_eq: 0, fp: 0 }.sealed()
    }

    /// D2248: the fingerprint moves with the sense, the operator, the collation, the hashed form and each element fact;
    /// equal Descs agree whatever `fp` held before `sealed` (one shape keeps one IR, one parked Node: V4R-101).
    #[test]
    fn fingerprints_tell_descs_apart() {
        let base = desc(true, 66, 0, INT4, 0).fp;
        let mut again = desc(true, 66, 0, INT4, 0);
        again.fp = 7;
        assert_eq!([desc(true, 66, 0, INT4, 0).fp, again.sealed().fp], [base, base], "equal Descs");
        let others = [
            ("ALL", desc(false, 66, 0, INT4, 0)),
            ("int4gt", desc(true, 147, 0, INT4, 0)),
            ("int4eq", desc(true, 65, 0, INT4, 0)),
            ("a collation", desc(true, 66, 100, INT4, 0)),
            ("a native set", desc(true, 66, 0, INT4, 1)),
            ("a hashed set", desc(true, 66, 0, INT4, 2)),
            ("NOT IN's set", desc(false, 66, 0, INT4, 2)),
            ("the element type", desc(true, 66, 0, ElemFacts { typ: 20, ..INT4 }, 0)),
            ("the element length", desc(true, 66, 0, ElemFacts { len: 8, ..INT4 }, 0)),
            ("a by-reference element", desc(true, 66, 0, ElemFacts { byval: false, ..INT4 }, 0)),
            ("the element alignment", desc(true, 66, 0, ElemFacts { align: b'd' as i8, ..INT4 }, 0)),
            ("a user key's set", Desc { user: 0x75_7365_72, ..desc(true, 66, 0, INT4, 1) }.sealed()),
        ];
        let mut seen = std::collections::BTreeMap::from([(base, "int4lt ANY")]);
        for (what, d) in others {
            if let Some(was) = seen.insert(d.fp, what) {
                panic!("{what}: the fingerprint of {was}");
            }
        }
    }

    /// D2248: `saop_eval` answers only from the Desc its site was lowered against; another fingerprint (a Desc of another
    /// statement's program) or no Desc is an internal error (fail closed).
    #[test]
    fn a_foreign_desc_fails_closed() {
        let (any, all) = (desc(true, 66, 0, INT4, 0), desc(false, 66, 0, INT4, 0));
        assert!(site(Some(&any), any.fp).is_ok_and(|d| d.use_or));
        assert_eq!(site(Some(&any), all.fp).err(), Some("the site's Desc is another program's"));
        assert_eq!(site(None, any.fp).err(), Some("the program owns no such site"));
    }

    /// D2248 (does not apply): expr.saop is the only run-time reader of a program's kept state (`hir::kept`), so its
    /// fingerprint closes the class; a new reader must put what it reads in the IR (or the program key) too.
    #[test]
    fn expr_saop_is_the_only_kept_reader() {
        let root = std::path::PathBuf::from(std::env::var_os("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR unset: run under cargo test (D793-b)")).join("src");
        let reads = |s: String| s.contains("owned_desc(") || s.lines().any(|l| l.contains("hir::") && l.contains("kept"));
        let (mut dirs, mut readers) = (vec![root.clone()], Vec::new());
        while let Some(d) = dirs.pop() {
            for p in std::fs::read_dir(&d).unwrap().map(|e| e.unwrap().path()) {
                if p.is_dir() {
                    dirs.push(p);
                } else if std::fs::read_to_string(&p).is_ok_and(reads) {
                    readers.push(p.strip_prefix(&root).unwrap().to_string_lossy().into_owned());
                }
            }
        }
        readers.sort();
        assert_eq!(readers, ["step/raw/saop_host.rs", "step/raw/saop_set.rs"], "a reader of a program's kept state beside expr.saop");
    }
}
