//! V4R-453: typcache entries a gravity pool claim is armed with. A pool thread has no catcache, so a hosted
//! aggregate's transition function that reaches `lookup_type_cache` there (array_cmp's element compare, an enum's order,
//! a range's subtype) found nothing it could load. The leader projects each entry the statement's aggregates need
//! (`image_closure`, at arena build, as ExecInitAgg resolves its catalog facts), the arena owns the images, and a claim
//! arms them (`arm`); `lookup_type_cache` then builds the worker's entry from its image instead of the catalog.
//!
//! Lifetime: an armed entry lives in this thread's per-claim map, never in the thread's persistent type cache (a pool
//! thread serves every session and database), and `disarm` (the claim guard's drop, on every path) drops the map and
//! forgets the images. An entry the claim handed to a caller outlives the claim only inside that caller's FmgrInfo
//! (`fn_extra`), which the arena owns: the arena drops its slots before its images (field order), after every claim.
use core::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;

use types_core::{InvalidOid, Oid};
use types_error::PgResult;

use crate::enum_cmp::{self, EnumData, EnumItem};
use crate::{lookup_type_cache, TypeCacheEntry, READY_PG_TYPE_DATA, TYPTYPE_MULTIRANGE, TYPTYPE_RANGE};

const TYPTYPE_ENUM: i8 = b'e' as i8;

pub struct TypcacheImage {
    type_id: Oid,
    type_id_hash: u32,
    typlen: i16,
    typbyval: bool,
    typalign: i8,
    typstorage: i8,
    typtype: i8,
    typrelid: Oid,
    typsubscript: Oid,
    typelem: Oid,
    typarray: Oid,
    typcollation: Oid,
    oids: [Oid; 10],   // btree_opf, btree_opintype, hash_opf, hash_opintype, eq_opr, lt_opr, gt_opr, cmp_proc, hash_proc, hash_extended_proc
    finfos: [Oid; 7],  // eq_opr, cmp_proc, hash_proc, hash_extended_proc, rng_cmp_proc, rng_canonical, rng_subdiff (0: unresolved)
    rng: [Oid; 4],     // rng_collation, rng_opfamily, rng_elemtype, rngtype
    flags: i32,
    ready: i32,
    enum_data: Option<(Oid, Box<[u64]>, Box<[EnumItem]>)>,
}

impl TypcacheImage {
    pub fn type_id(&self) -> Oid {
        self.type_id
    }

    pub fn procs(&self) -> impl Iterator<Item = Oid> + '_ {
        self.finfos.iter().copied().filter(|&f| f != InvalidOid)
    }

    /// A range's collation (its subtype compare's; 0 for any other type): the arena resolves it on the leader.
    pub fn rng_collation(&self) -> Oid {
        self.rng[0]
    }
}

pub fn image_closure(type_id: Oid, flags: i32, out: &mut Vec<TypcacheImage>) -> PgResult<()> {
    if type_id == InvalidOid || out.iter().any(|i| i.type_id == type_id) {
        return Ok(());
    }
    let e = lookup_type_cache(type_id, flags)?;
    if e.typtype() == TYPTYPE_ENUM {
        // read afresh: a member added since the leader's last load would send the claim to the catalog
        enum_cmp::reload(&e)?;
    }
    let fo = |c: &RefCell<types_fmgr::FmgrInfo>| c.borrow().fn_oid;
    let img = TypcacheImage {
        type_id,
        type_id_hash: e.type_id_hash,
        typlen: e.typlen(),
        typbyval: e.typbyval(),
        typalign: e.typalign(),
        typstorage: e.typstorage(),
        typtype: e.typtype(),
        typrelid: e.typrelid(),
        typsubscript: e.typsubscript(),
        typelem: e.typelem(),
        typarray: e.typarray(),
        typcollation: e.typcollation(),
        oids: [e.btree_opf(), e.btree_opintype(), e.hash_opf(), e.hash_opintype(), e.eq_opr(), e.lt_opr(), e.gt_opr(), e.cmp_proc(), e.hash_proc(), e.hash_extended_proc()],
        finfos: [fo(&e.eq_opr_finfo), fo(&e.cmp_proc_finfo), fo(&e.hash_proc_finfo), fo(&e.hash_extended_proc_finfo), fo(&e.rng_cmp_proc_finfo), fo(&e.rng_canonical_finfo), fo(&e.rng_subdiff_finfo)],
        rng: [e.rng_collation.get(), e.rng_opfamily.get(), e.rng_elemtype.borrow().as_ref().map_or(InvalidOid, |r| r.type_id), e.rngtype.borrow().as_ref().map_or(InvalidOid, |r| r.type_id)],
        flags: e.flags.get(),
        // a range's RANGE_INFO is never warm-ready (C re-checks the element's pg_type data on each hit, compute_ready);
        // loaded on the leader, it is part of the claim's snapshot
        ready: e.ready.get() | if e.typtype() == TYPTYPE_RANGE && e.rng_elemtype.borrow().is_some() { crate::TYPECACHE_RANGE_INFO } else { 0 },
        enum_data: e.enum_data.borrow().as_ref().map(EnumData::parts).transpose()?,
    };
    let (elem, sub, range) = (if e.typtype() == TYPTYPE_RANGE || e.typtype() == TYPTYPE_MULTIRANGE { InvalidOid } else { e.typelem() }, img.rng[2], img.rng[3]);
    out.push(img); // alloc: Exec
    image_closure(elem, flags, out)?;
    image_closure(sub, flags, out)?;
    image_closure(range, flags | crate::TYPECACHE_RANGE_INFO, out)
}

struct Armed {
    images: &'static [TypcacheImage],
    map: HashMap<Oid, Rc<TypeCacheEntry>>,
}

std::thread_local! {
    static ARMED: RefCell<Option<Armed>> = const { RefCell::new(None) };
    static STATEMENT: RefCell<Option<Armed>> = const { RefCell::new(None) };
}

pub fn arm_statement(images: &'static [TypcacheImage]) {
    STATEMENT.with(|a| {
        let mut a = a.borrow_mut();
        match a.as_mut() {
            Some(cur) if core::ptr::eq(cur.images, images) => {}
            Some(cur) => {
                cur.images = images;
                cur.map.clear();
            }
            None => *a = Some(Armed { images, map: HashMap::new() }), // alloc: Exec
        }
    });
}

/// the claim's end for the statement slot (no allocation, no panic: the claim guard's drop, an unwind too).
pub fn disarm_statement() {
    let _ = STATEMENT.try_with(|a| {
        if let Ok(mut a) = a.try_borrow_mut() {
            if let Some(cur) = a.as_mut() {
                cur.images = &[];
                cur.map.clear();
            }
        }
    });
}

/// whether this thread's claim has statement images armed (the catalog-miss check's strict mode).
pub fn statement_armed() -> bool {
    STATEMENT.try_with(|a| a.try_borrow().is_ok_and(|a| a.as_ref().is_some_and(|c| !c.images.is_empty()))).unwrap_or(false)
}

pub fn arm(images: &'static [TypcacheImage]) {
    ARMED.with(|a| {
        let mut a = a.borrow_mut();
        match a.as_mut() {
            Some(cur) if core::ptr::eq(cur.images, images) => {}
            Some(cur) => {
                cur.images = images;
                cur.map.clear();
            }
            None => *a = Some(Armed { images, map: HashMap::new() }), // alloc: Exec
        }
    });
}

/// Whether this thread's claim is armed with `images` (the hosted entry's per-call check: one thread-local read).
#[inline]
pub fn armed_with(images: &'static [TypcacheImage]) -> bool {
    ARMED.try_with(|a| a.try_borrow().is_ok_and(|a| a.as_ref().is_some_and(|c| core::ptr::eq(c.images, images)))).unwrap_or(false)
}

/// The claim's end (the pool's claim guard, on an unwind too): the images forgotten and every armed entry dropped. No
/// allocation and no panic: a thread-local store and the map's drop (entries' FmgrInfos, global-heap boxes).
pub fn disarm() {
    let _ = ARMED.try_with(|a| {
        if let Ok(mut a) = a.try_borrow_mut() {
            if let Some(cur) = a.as_mut() {
                cur.images = &[];
                cur.map.clear();
            }
        }
    });
}

pub fn enum_type_of(enum_oid: Oid) -> Option<Oid> {
    let has = |i: &&TypcacheImage| i.enum_data.as_ref().is_some_and(|(_, _, items)| enum_cmp::holds(items, enum_oid));
    let find = |slot: &'static std::thread::LocalKey<RefCell<Option<Armed>>>| slot.try_with(|a| a.try_borrow().ok().and_then(|a| a.as_ref().and_then(|c| c.images.iter().find(has).map(|i| i.type_id)))).ok().flatten();
    find(&ARMED).or_else(|| find(&STATEMENT))
}

pub(crate) fn lookup(type_id: Oid, flags: i32) -> PgResult<Option<Rc<TypeCacheEntry>>> {
    match lookup_in(&ARMED, type_id, flags)? {
        Some(e) => Ok(Some(e)),
        None => lookup_in(&STATEMENT, type_id, flags),
    }
}

fn lookup_in(slot: &'static std::thread::LocalKey<RefCell<Option<Armed>>>, type_id: Oid, flags: i32) -> PgResult<Option<Rc<TypeCacheEntry>>> {
    let (hit, img) = slot.with(|a| match a.borrow().as_ref() {
        Some(cur) if !cur.images.is_empty() => (cur.map.get(&type_id).cloned(), cur.images.iter().find(|i| i.type_id == type_id)),
        _ => (None, None),
    });
    let Some(img) = img else { return Ok(None) };
    if (flags | READY_PG_TYPE_DATA) & !img.ready != 0 {
        return Ok(None);
    }
    if let Some(e) = hit {
        return Ok(Some(e));
    }
    let e = Rc::new(entry(img)?);
    slot.with(|a| {
        if let Some(cur) = a.borrow_mut().as_mut() {
            cur.map.insert(type_id, Rc::clone(&e)); // alloc: Exec
        }
    });
    Ok(Some(e))
}

fn entry(img: &'static TypcacheImage) -> PgResult<TypeCacheEntry> {
    let e = TypeCacheEntry::new(img.type_id, img.type_id_hash);
    e.typlen.set(img.typlen);
    e.typbyval.set(img.typbyval);
    e.typalign.set(img.typalign);
    e.typstorage.set(img.typstorage);
    e.typtype.set(img.typtype);
    e.typrelid.set(img.typrelid);
    e.typsubscript.set(img.typsubscript);
    e.typelem.set(img.typelem);
    e.typarray.set(img.typarray);
    e.typcollation.set(img.typcollation);
    let [bo, bi, ho, hi, eq, lt, gt, cmp, hp, hx] = img.oids;
    for (c, v) in [(&e.btree_opf, bo), (&e.btree_opintype, bi), (&e.hash_opf, ho), (&e.hash_opintype, hi), (&e.eq_opr, eq), (&e.lt_opr, lt), (&e.gt_opr, gt), (&e.cmp_proc, cmp), (&e.hash_proc, hp), (&e.hash_extended_proc, hx)] {
        c.set(v);
    }
    let cells = [&e.eq_opr_finfo, &e.cmp_proc_finfo, &e.hash_proc_finfo, &e.hash_extended_proc_finfo, &e.rng_cmp_proc_finfo, &e.rng_canonical_finfo, &e.rng_subdiff_finfo];
    for (c, &f) in cells.into_iter().zip(&img.finfos) {
        if f != InvalidOid {
            *c.borrow_mut() = fmgr_seams::fmgr_info::call(f)?;
        }
    }
    let [coll, opf, sub, range] = img.rng;
    e.rng_collation.set(coll);
    e.rng_opfamily.set(opf);
    if sub != InvalidOid {
        *e.rng_elemtype.borrow_mut() = Some(lookup_type_cache(sub, 0)?);
    }
    if range != InvalidOid {
        *e.rngtype.borrow_mut() = Some(lookup_type_cache(range, 0)?);
    }
    if let Some((base, words, items)) = &img.enum_data {
        *e.enum_data.borrow_mut() = Some(EnumData::armed(*base, &**words, &**items));
    }
    e.flags.set(img.flags);
    e.ready.set(img.ready);
    Ok(e)
}
