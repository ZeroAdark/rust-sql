use datum::Datum;
use mcx::{Mcx, PgVec};
use std::cell::Cell;
use types_core::{AttrNumber, InvalidOid, Oid, FLOAT4OID};
use types_error::{PgError, PgResult, ERRCODE_DATA_CORRUPTED};
use types_tuple::HeapTupleData;

pub const STATISTIC_NUM_SLOTS: usize = 5;
pub const ANUM_PG_STATISTIC_STANUMBERS1: i32 = 22;
pub const ANUM_PG_STATISTIC_STAVALUES1: i32 = 27;
pub const ATTSTATSSLOT_VALUES: i32 = 0x01;
pub const ATTSTATSSLOT_NUMBERS: i32 = 0x02;

pub type GetAttAvgWidthHook = fn(Oid, AttrNumber) -> i32;

thread_local! {
    static GET_ATTAVGWIDTH_HOOK: Cell<Option<GetAttAvgWidthHook>> = const { Cell::new(None) };
}

pub fn set_get_attavgwidth_hook(hook: Option<GetAttAvgWidthHook>) -> Option<GetAttAvgWidthHook> {
    GET_ATTAVGWIDTH_HOOK.with(|cell| cell.replace(hook))
}

pub fn get_attavgwidth(relid: Oid, attnum: AttrNumber) -> PgResult<i32> {
    if let Some(hook) = GET_ATTAVGWIDTH_HOOK.with(|cell| cell.get()) {
        let stawidth = hook(relid, attnum);
        if stawidth > 0 {
            return Ok(stawidth);
        }
    }
    if let Some(stawidth) = syscache_seams::pg_statistic_stawidth::call(relid, attnum, false)? {
        if stawidth > 0 {
            return Ok(stawidth);
        }
    }
    Ok(0)
}

// `values_arr` is kept only while by-reference `values` datums point into it.
#[derive(Debug)]
pub struct AttStatsSlot<'mcx> {
    pub staop: Oid,
    pub stacoll: Oid,
    pub valuetype: Oid,
    pub values: PgVec<'mcx, Datum>,
    pub numbers: PgVec<'mcx, f32>,
    pub values_arr: Option<PgVec<'mcx, u8>>,
}

pub fn get_attstatsslot<'mcx>(
    mcx: Mcx<'mcx>,
    statstuple: &HeapTupleData<'_>,
    reqkind: i32,
    reqop: Oid,
    flags: i32,
) -> PgResult<Option<AttStatsSlot<'mcx>>> {
    let stats = syscache_seams::pg_statistic_slot_shape::call(statstuple);
    let Some(i) = (0..STATISTIC_NUM_SLOTS).find(|&i| {
        stats.stakind[i] as i32 == reqkind && (reqop == InvalidOid || stats.staop[i] == reqop)
    }) else {
        return Ok(None);
    };
    let mut sslot = AttStatsSlot {
        staop: stats.staop[i],
        stacoll: stats.stacoll[i],
        valuetype: InvalidOid,
        values: PgVec::new_in(mcx),
        numbers: PgVec::new_in(mcx),
        values_arr: None,
    };

    if flags & ATTSTATSSLOT_VALUES != 0 {
        let statarray = syscache_seams::pg_statistic_slot_array_image::call(
            mcx,
            statstuple,
            ANUM_PG_STATISTIC_STAVALUES1 + i as i32,
        )?;
        let arrayelemtype = array_header_field(&statarray, ARR_ELEMTYPE_OFF)? as Oid;
        sslot.valuetype = arrayelemtype;
        let Some(type_form) = syscache_seams::lookup_pg_type_shape::call(arrayelemtype)? else {
            return Err(crate::typ::type_lookup_failed(arrayelemtype));
        };
        sslot.values = datum::array_build::deconstruct_array_image(
            mcx,
            &statarray,
            type_form.typlen,
            type_form.typbyval,
            type_form.typalign as u8,
        )?;
        if !type_form.typbyval {
            sslot.values_arr = Some(statarray);
        } else {
            drop(statarray);
        }
    }

    if flags & ATTSTATSSLOT_NUMBERS != 0 {
        let statarray = syscache_seams::pg_statistic_slot_array_image::call(
            mcx,
            statstuple,
            ANUM_PG_STATISTIC_STANUMBERS1 + i as i32,
        )?;
        let ndim = array_header_field(&statarray, ARR_NDIM_OFF)?;
        let dataoffset = array_header_field(&statarray, ARR_DATAOFFSET_OFF)?;
        let elemtype = array_header_field(&statarray, ARR_ELEMTYPE_OFF)? as Oid;
        let narrayelem = array_header_field(&statarray, ARR_DIMS_OFF)?;
        if ndim != 1 || narrayelem <= 0 || dataoffset != 0 || elemtype != FLOAT4OID {
            return Err(stanumbers_not_float4_array());
        }
        let data = statarray
            .get(ARR_1D_NONULLS_DATA_OFF..)
            .filter(|d| d.len() >= narrayelem as usize * 4)
            .ok_or_else(stanumbers_not_float4_array)?;
        let mut numbers: PgVec<'mcx, f32> = mcx::vec_with_capacity_in(mcx, narrayelem as usize)?;
        numbers.extend(
            data[..narrayelem as usize * 4]
                .chunks_exact(4)
                .map(|b| f32::from_ne_bytes(b.try_into().unwrap())),
        );
        sslot.numbers = numbers;
        drop(statarray);
    }

    Ok(Some(sslot))
}

const ARR_NDIM_OFF: usize = 4;
const ARR_DATAOFFSET_OFF: usize = 8;
const ARR_ELEMTYPE_OFF: usize = 12;
const ARR_DIMS_OFF: usize = 16;
const ARR_1D_NONULLS_DATA_OFF: usize = 24;

// An image too short for the word is a catchable error rather than an out-of-bounds read.
fn array_header_field(image: &[u8], off: usize) -> PgResult<i32> {
    match image.get(off..off + 4) {
        Some(b) => Ok(i32::from_ne_bytes(b.try_into().unwrap())),
        None => Err(Box::new(
            PgError::error("get_attstatsslot: malformed pg_statistic array image")
                .with_sqlstate(ERRCODE_DATA_CORRUPTED),
        )),
    }
}

#[cold]
#[inline(never)]
fn stanumbers_not_float4_array() -> Box<PgError> {
    Box::new(PgError::error("stanumbers is not a 1-D float4 array"))
}

pub fn free_attstatsslot(sslot: AttStatsSlot<'_>) {
    drop(sslot);
}
