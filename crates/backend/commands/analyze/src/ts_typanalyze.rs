//! ts_typanalyze.c: compute_tsvector_stats — most-common-lexemes MCELEM via
//! Lossy Counting.

use datum::Datum;
use mcx::{Mcx, MemoryContext, PgVec};
use types_error::PgResult;

use crate::dynahash_order::{DynaHash, EntrySlab};
use crate::{ComputeStats, FetchSource, VacAttrStats};

const TEXT_EQUAL_OPERATOR: types_core::Oid = 98;
const DEFAULT_COLLATION_OID: types_core::Oid = 100;
const TEXTOID: types_core::Oid = 25;

pub(crate) fn setup(stats: &mut VacAttrStats<'_>) -> PgResult<bool> {
    if stats.attstattarget < 0 {
        stats.attstattarget = guc_tables::vars::default_statistics_target.read();
    }
    stats.compute = ComputeStats::TsVector;
    stats.minrows = 300 * stats.attstattarget;
    Ok(true)
}

struct TrackItem<'k> {
    // None once pruned (ts_typanalyze.c:468 pfree of the lexeme copy).
    key: Option<PgVec<'k, u8>>,
    frequency: i32,
    delta: i32,
}

// ts_typanalyze.c:127 trackCmp.
fn track_cmp(a: i32, b: i32) -> i32 {
    if a == b {
        0
    } else if a > b {
        -1
    } else {
        1
    }
}

// ts_typanalyze.c:117 lexeme_compare.
fn lexeme_compare(a: &[u8], b: &[u8]) -> i32 {
    if a.len() == b.len() {
        match a.cmp(b) {
            core::cmp::Ordering::Less => -1,
            core::cmp::Ordering::Equal => 0,
            core::cmp::Ordering::Greater => 1,
        }
    } else if a.len() > b.len() {
        1
    } else {
        -1
    }
}

pub(crate) fn compute_tsvector_stats<'mcx>(
    anl_mcx: Mcx<'mcx>,
    col_mcx: Mcx<'_>,
    stats: &mut VacAttrStats<'mcx>,
    src: &FetchSource<'_, '_>,
    samplerows: i32,
) -> PgResult<()> {
    let num_mcelem_target = stats.attstattarget * 10;
    let bucket_width = (num_mcelem_target + 10) * 1000 / 7;

    // aset: pruned lexeme copies and chain growth free per C's pfree.
    let key_cx = col_mcx.context().new_child("compute_tsvector_stats lexemes");
    let key_mcx = key_cx.mcx();
    let mut items: EntrySlab<'_, TrackItem<'_>> = EntrySlab::new(col_mcx);
    let mut lexemes_tab = DynaHash::new(key_mcx, num_mcelem_target as i64)?;

    let mut b_current = 1i32;
    let mut lexeme_no = 0i64;
    let mut null_cnt = 0i32;
    let mut total_width = 0.0f64;

    let mut row_scratch = MemoryContext::new_wholesale("compute_tsvector_stats row scratch");
    for vector_no in 0..samplerows {
        // ts_typanalyze.c:207: vacuum_delay_point(true) per sample row.
        commands_vacuum::vacuum_delay_point(true)?;
        let (value, isnull) = src.fetch(vector_no as usize, stats.tupattnum);
        if isnull {
            null_cnt += 1;
            continue;
        }
        {
            let row_mcx = row_scratch.mcx();
            let p = value.as_usize() as *const u8;
            // SAFETY: non-null varlena datum readable through its header.
            let raw =
                unsafe { core::slice::from_raw_parts(p, types_tuple::varatt::varsize_any(p)) };
            // C uses the toasted width (VARSIZE_ANY of the stored datum).
            total_width += raw.len() as f64;

            let img: &[u8] = if raw[0] & 0x03 == 0 {
                raw
            } else {
                let v = detoast_seams::detoast_attr::call(row_mcx, raw)?;
                adt_multirangetypes::leak_image(v)
            };
            let vector = adt_tsvector_core::layout::TsVec { payload: &img[4..] };
            for j in 0..vector.size() {
                let lexeme = vector.lexeme(vector.entry(j));
                let h = hashfn::hash_bytes(lexeme);
                let mut found = None;
                for &(hh, id) in lexemes_tab.enter(h)? {
                    if hh == h && items[id].key.as_deref() == Some(lexeme) {
                        found = Some(id);
                        break;
                    }
                }
                match found {
                    Some(idx) => items[idx].frequency += 1,
                    None => {
                        let key = mcx::slice_in(key_mcx, lexeme)?;
                        let id =
                            items.alloc(TrackItem { key: Some(key), frequency: 1, delta: b_current - 1 });
                        lexemes_tab.insert(h, id);
                    }
                }
                lexeme_no += 1;
                if lexeme_no % bucket_width as i64 == 0 {
                    lexemes_tab.prune(|id| {
                        let it = &mut items[id];
                        let keep = it.frequency + it.delta > b_current;
                        if !keep {
                            it.key = None;
                            items.release(id);
                        }
                        keep
                    });
                    b_current += 1;
                }
            }
        }
        row_scratch.reset();
    }

    if null_cnt < samplerows {
        let nonnull_cnt = samplerows - null_cnt;
        stats.stats_valid = true;
        stats.stanullfrac = (null_cnt as f64 / samplerows as f64) as f32;
        stats.stawidth = (total_width / nonnull_cnt as f64) as i32;
        stats.stadistinct = (-1.0 * (1.0 - stats.stanullfrac as f64)) as f32;

        let cutoff_freq = 9 * lexeme_no / bucket_width as i64;

        let mut sort_table: PgVec<'_, u32> =
            mcx::vec_with_capacity_in(col_mcx, lexemes_tab.len())?;
        let mut minfreq = lexeme_no;
        let mut maxfreq = 0i64;
        for id in lexemes_tab.seq() {
            let it = &items[id];
            if it.frequency as i64 > cutoff_freq {
                sort_table.push(id);
                minfreq = minfreq.min(it.frequency as i64);
                maxfreq = maxfreq.max(it.frequency as i64);
            }
        }
        let track_len = sort_table.len() as i32;

        // ts_typanalyze.c:341: emit some statistics for debug purposes.
        elog::ereport(types_error::DEBUG3)
            .errmsg(format!(
                "tsvector_stats: target # mces = {}, bucket width = {}, # lexemes = {}, \
                 hashtable size = {}, usable entries = {}",
                num_mcelem_target,
                bucket_width,
                lexeme_no,
                lexemes_tab.len(),
                track_len
            ))
            .finish(types_error::ErrorLocation::new(file!(), line!() as i32, "compute_tsvector_stats"))?;

        let mut num_mcelem = num_mcelem_target;
        if num_mcelem < track_len {
            // ts_typanalyze.c:352 qsort_interruptible(trackCmp) over the
            // hash_seq order: equal frequencies at the cutoff keep C's set.
            ::pg_qsort::pg_qsort_interruptible(
                &mut sort_table[..],
                |&a, &b| track_cmp(items[a].frequency, items[b].frequency),
                postgres_seams::check_for_interrupts::call,
            )?;
            minfreq = items[sort_table[num_mcelem as usize - 1]].frequency as i64;
        } else {
            num_mcelem = track_len;
        }

        if num_mcelem > 0 {
            let prefix = &mut sort_table[..num_mcelem as usize];
            let lexeme_of = |id: u32| items[id].key.as_deref().expect("retained lexeme");
            ::pg_qsort::pg_qsort_interruptible(
                prefix,
                |&a, &b| lexeme_compare(lexeme_of(a), lexeme_of(b)),
                postgres_seams::check_for_interrupts::call,
            )?;

            let mut mcelem_values: PgVec<'mcx, Datum> =
                mcx::vec_with_capacity_in(anl_mcx, num_mcelem as usize)?;
            let mut mcelem_freqs: PgVec<'mcx, f32> =
                mcx::vec_with_capacity_in(anl_mcx, num_mcelem as usize + 2)?;
            for &id in prefix.iter() {
                mcelem_values.push(text_datum_in(anl_mcx, lexeme_of(id))?);
                mcelem_freqs.push((items[id].frequency as f64 / nonnull_cnt as f64) as f32);
            }
            mcelem_freqs.push((minfreq as f64 / nonnull_cnt as f64) as f32);
            mcelem_freqs.push((maxfreq as f64 / nonnull_cnt as f64) as f32);

            stats.stakind[0] = crate::array_typanalyze::STATISTIC_KIND_MCELEM;
            stats.staop[0] = TEXT_EQUAL_OPERATOR;
            stats.stacoll[0] = DEFAULT_COLLATION_OID;
            stats.stanumbers[0] = mcelem_freqs;
            stats.stavalues[0] = mcelem_values;
            stats.stavalues_set[0] = true;
            stats.statypid[0] = TEXTOID;
            stats.statyplen[0] = -1;
            stats.statypbyval[0] = false;
            stats.statypalign[0] = b'i';
        }
    } else {
        stats.stats_valid = true;
        stats.stanullfrac = 1.0;
        stats.stawidth = 0;
        stats.stadistinct = 0.0;
    }
    Ok(())
}

fn text_datum_in<'m>(mcx: Mcx<'m>, s: &[u8]) -> PgResult<Datum> {
    let mut img: PgVec<'m, u8> = mcx::vec_with_capacity_in(mcx, s.len() + datum::VARHDRSZ)?;
    mcx::vec_append_bytes(&mut img, &datum::set_varsize_4b(s.len() + datum::VARHDRSZ))?;
    mcx::vec_append_bytes(&mut img, s)?;
    Ok(Datum::from_usize(adt_multirangetypes::leak_image(img).as_ptr() as usize))
}
