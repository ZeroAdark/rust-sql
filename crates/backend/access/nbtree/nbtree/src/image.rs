
use std::collections::HashMap;

use ::types_core::{Oid, BTREE_AM_OID, INVALID_PROC_NUMBER};
use ::types_error::PgResult;
use ::types_nbtree::{BTMetaPageData, SkipSupportData, BTORDER_PROC};
use ::types_rel::Relation;
use ::types_storage::RelFileLocator;
use ::types_tuple::tupdesc::{CompactAttribute, FormData_pg_attribute};

#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct CompactAttrImage {
    pub attcacheoff: i32,
    pub attlen: i16,
    pub attbyval: bool,
    pub attispackable: bool,
    pub atthasmissing: bool,
    pub attisdropped: bool,
    pub attgenerated: bool,
    pub attnullability: i8,
    pub attalignby: u8,
}

impl CompactAttrImage {
    fn of(a: &CompactAttribute) -> Self {
        CompactAttrImage {
            attcacheoff: a.attcacheoff.get(),
            attlen: a.attlen,
            attbyval: a.attbyval,
            attispackable: a.attispackable,
            atthasmissing: a.atthasmissing,
            attisdropped: a.attisdropped,
            attgenerated: a.attgenerated,
            attnullability: a.attnullability,
            attalignby: a.attalignby,
        }
    }

    pub(crate) fn thaw(&self) -> CompactAttribute {
        CompactAttribute {
            attcacheoff: core::cell::Cell::new(self.attcacheoff),
            attlen: self.attlen,
            attbyval: self.attbyval,
            attispackable: self.attispackable,
            atthasmissing: self.atthasmissing,
            attisdropped: self.attisdropped,
            attgenerated: self.attgenerated,
            attnullability: self.attnullability,
            attalignby: self.attalignby,
        }
    }
}

pub(crate) type FamilyKey = (Oid, Oid, Oid, i16);

#[derive(Clone)]
pub struct IndexImage {
    pub(crate) oid: Oid,
    pub(crate) name: String,
    pub(crate) indnatts: i32,
    pub(crate) indnkeyatts: i32,
    pub(crate) td_natts: i32,
    pub(crate) td_typeid: Oid,
    pub(crate) td_typmod: i32,
    pub(crate) attrs: Vec<FormData_pg_attribute>,
    pub(crate) compact: Vec<CompactAttrImage>,
    pub(crate) opcintype: Vec<Oid>,
    pub(crate) opfamily: Vec<Oid>,
    pub(crate) indcollation: Vec<Oid>,
    pub(crate) indoption: Vec<i16>,
    pub(crate) locator: RelFileLocator,
    pub(crate) permanent: bool,
    /// Always false: capture refuses indexes in backend-local buffers.
    pub(crate) local_buffers: bool,
    pub(crate) metapage: Option<BTMetaPageData>,
    pub(crate) pgstat_enabled: bool,
    pub(crate) families: Vec<Oid>,
    pub(crate) amproc: HashMap<FamilyKey, Oid>,
    pub(crate) amop: HashMap<FamilyKey, Oid>,
    pub(crate) opcode: HashMap<Oid, Oid>,
    pub(crate) types: HashMap<Oid, (i16, bool, i8)>,
    pub(crate) skip: Vec<((Oid, Oid), Option<SkipSupportData>)>,
}

const _: fn() = || {
    fn shareable<T: Send + Sync + 'static>() {}
    shareable::<IndexImage>();
};

impl IndexImage {
    /// Freezes `rel`. `Ok(None)`: not imageable (not a btree index, an index
    /// in backend-local buffers, inconsistent opclass arrays, an operator
    /// family with ambiguous catalog rows, or skip support that cannot be
    /// prepared); the caller keeps the relcache path. Reads the metapage when
    /// the relation has none cached, priming the relation's cache as a scan
    /// would.
    pub fn capture(rel: &Relation<'_>) -> PgResult<Option<IndexImage>> {
        let Some(index) = rel.rd_index.as_ref() else {
            return Ok(None);
        };
        if rel.rd_rel.relam != BTREE_AM_OID {
            return Ok(None);
        }
        if rel.uses_local_buffers() || rel.rd_islocaltemp || rel.rd_backend != INVALID_PROC_NUMBER
        {
            return Ok(None);
        }
        let natts = index.indnatts as i32;
        let nkey = index.indnkeyatts as i32;
        if nkey <= 0 || nkey > natts {
            return Ok(None);
        }
        let nkey_us = nkey as usize;
        if rel.rd_opfamily.len() < nkey_us
            || rel.rd_opcintype.len() < nkey_us
            || rel.rd_indcollation.len() < nkey_us
            || rel.rd_indoption.len() < nkey_us
        {
            return Ok(None);
        }
        let td = &*rel.rd_att;
        if td.natts != natts || td.compact_attrs.len() != natts as usize {
            return Ok(None);
        }

        let metapage = match crate::page::prime_amcache(rel)? {
            Some(_uncached_empty) => None,
            None => rel.rd_amcache.get(),
        };

        let opfamily: Vec<Oid> = rel.rd_opfamily.iter().copied().collect();
        let opcintype: Vec<Oid> = rel.rd_opcintype.iter().copied().collect();

        let mut families: Vec<Oid> = Vec::new();
        families
            .try_reserve_exact(nkey_us)
            .map_err(|_| ::mcx::oom_named("nbtree", nkey_us * core::mem::size_of::<Oid>()))?;
        families.extend_from_slice(&opfamily[..nkey_us]);
        families.sort_unstable();
        families.dedup();

        let mut amproc: HashMap<FamilyKey, Oid> = HashMap::new();
        let mut amop: HashMap<FamilyKey, Oid> = HashMap::new();
        let mut opcode: HashMap<Oid, Oid> = HashMap::new();
        let mut typeids: Vec<Oid> = Vec::new();
        {
            let cx = ::mcx::MemoryContext::new("index image capture");
            for &of in &families {
                let (procs, _ordered) =
                    ::syscache_seams::lookup_pg_amproc_rows::call(cx.mcx(), of)?;
                for r in procs.iter() {
                    let k = (r.amprocfamily, r.amproclefttype, r.amprocrighttype, r.amprocnum);
                    if r.amprocfamily != of || *amproc.entry(k).or_insert(r.amproc) != r.amproc {
                        return Ok(None);
                    }
                    typeids.extend([r.amproclefttype, r.amprocrighttype]);
                }
                let (ops, _ordered) = ::syscache_seams::lookup_pg_amop_rows::call(cx.mcx(), of)?;
                for r in ops.iter() {
                    let k = (r.amopfamily, r.amoplefttype, r.amoprighttype, r.amopstrategy);
                    if r.amopfamily != of || *amop.entry(k).or_insert(r.amopopr) != r.amopopr {
                        return Ok(None);
                    }
                    typeids.extend([r.amoplefttype, r.amoprighttype]);
                    if let std::collections::hash_map::Entry::Vacant(e) = opcode.entry(r.amopopr) {
                        e.insert(::lsyscache::get_opcode(r.amopopr)?);
                    }
                }
            }
        }
        typeids.extend(opcintype.iter().copied());
        typeids.extend(td.attrs.iter().map(|a| a.atttypid));
        typeids.retain(|t| *t != 0);
        typeids.sort_unstable();
        typeids.dedup();
        let mut types = HashMap::new();
        types.try_reserve(typeids.len()).map_err(|_| {
            ::mcx::oom_named("nbtree", typeids.len() * core::mem::size_of::<(Oid, (i16, bool, i8))>())
        })?;
        for t in typeids {
            types.insert(t, ::lsyscache::get_typlenbyvalalign(t)?);
        }

        let mut skip: Vec<((Oid, Oid), Option<SkipSupportData>)> = Vec::new();
        for i in 0..nkey_us {
            let key = (opfamily[i], opcintype[i]);
            if skip.iter().any(|(k, _)| *k == key) {
                continue;
            }
            match ::skipsupport::prepare_skip_support_from_opclass(key.0, key.1, false) {
                Ok(s) => skip.push((key, s)),
                Err(_) => return Ok(None),
            }
        }

        Ok(Some(IndexImage {
            oid: rel.rd_id,
            name: rel.name().to_owned(),
            indnatts: natts,
            indnkeyatts: nkey,
            td_natts: td.natts,
            td_typeid: td.tdtypeid,
            td_typmod: td.tdtypmod,
            attrs: td.attrs.iter().copied().collect(),
            compact: td.compact_attrs.iter().map(CompactAttrImage::of).collect(),
            opcintype,
            opfamily,
            indcollation: rel.rd_indcollation.iter().copied().collect(),
            indoption: rel.rd_indoption.iter().copied().collect(),
            locator: rel.rd_locator.get(),
            permanent: rel.is_permanent(),
            local_buffers: false,
            metapage,
            pgstat_enabled: rel.pgstat_enabled.get(),
            families,
            amproc,
            amop,
            opcode,
            types,
            skip,
        }))
    }

    pub fn oid(&self) -> Oid {
        self.oid
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn locator(&self) -> RelFileLocator {
        self.locator
    }

    pub fn families(&self) -> &[Oid] {
        &self.families
    }

    /// Every function the image could invoke through the function manager:
    /// the families' order procs (BTORDER_PROC rows, same- and cross-type)
    /// and the operators' implementing functions, sorted and deduplicated,
    /// for a caller's allowlist check. The other support procedures are never
    /// called by the read path (skip support is resolved at capture).
    pub fn proc_oids(&self) -> impl Iterator<Item = Oid> + '_ {
        let mut v: Vec<Oid> = self
            .amproc
            .iter()
            .filter(|(k, _)| k.3 == BTORDER_PROC as i16)
            .map(|(_, p)| p)
            .chain(self.opcode.values())
            .copied()
            .filter(|p| *p != 0)
            .collect();
        v.sort_unstable();
        v.dedup();
        v.into_iter()
    }

    pub(crate) fn invocable(&self, proc: Oid) -> bool {
        proc != 0
            && (self.opcode.values().any(|p| *p == proc)
                || self
                    .amproc
                    .iter()
                    .any(|(k, p)| k.3 == BTORDER_PROC as i16 && *p == proc))
    }
}
