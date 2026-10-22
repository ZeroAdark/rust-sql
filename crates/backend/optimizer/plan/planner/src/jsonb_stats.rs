//! pgrust-only: examine_variable's expression leg turns a `->` / `->>` / `#>` / `#>>` chain over a jsonb
//! column with a path slot (optionally under an I/O cast) into a PgStatisticBundle, so C's estimators run
//! unchanged; `?` / `?|` / `?&` read path presence. A path with no record gets the blob's untracked bound,
//! never ~0 (an underestimate is how a nested loop gets picked), unless the blob is FLAG_COMPLETE.

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;

use adt_jsonb_pathstats as fmt;
use datum::Datum;
use mcx::PgVec;
use syscache_seams::{PgStatisticBundle, PgStatisticSlotData};
use types_core::Oid;
use types_error::PgResult;
use types_nodes::Node;
use types_pathnodes::NodeId;

use crate::run::PlannerRun;
use crate::selfuncs::{
    all_rows_selectable, get_att_stats, leak_bundle, rte_at, VariableStatData,
    STATISTIC_KIND_HISTOGRAM, STATISTIC_KIND_MCV,
};

const JSONBOID: Oid = 3802;
const TEXTOID: Oid = 25;
const TEXTARRAYOID: Oid = 1009;
const TEXT_EQ_OPERATOR: Oid = 98;
const DEFAULT_COLLATION_OID: Oid = 100;
const F_JSONB_OBJECT_FIELD: Oid = 3478;
const F_JSONB_OBJECT_FIELD_TEXT: Oid = 3214;
const F_JSONB_EXTRACT_PATH: Oid = 3217;
const F_JSONB_EXTRACT_PATH_TEXT: Oid = 3940;
const F_JSONB_EXISTS: Oid = 4047;
const F_JSONB_EXISTS_ANY: Oid = 4048;
const F_JSONB_EXISTS_ALL: Oid = 4049;

pub(crate) fn enabled() -> bool {
    guc_tables::backing::pgrust_jsonb_path_stats()
}

struct Chain<'a> {
    varno: i32,
    varattno: i16,
    segs_rev: std::vec::Vec<&'a [u8]>,
    terminal_text: bool,
}

fn strip_relabel(mut n: Node<'_>) -> Node<'_> {
    while let Some(r) = n.as_relabel_type() {
        n = r.arg;
    }
    n
}

fn resolve_chain<'mcx>(run: &PlannerRun<'mcx>, node: Node<'mcx>) -> Option<Chain<'mcx>> {
    let mut segs_rev: std::vec::Vec<&[u8]> = std::vec::Vec::new();
    let mut terminal_text = false;
    let mut cur = node;
    let mut first = true;
    while let Some(op) = cur.as_op_expr() {
        if op.args.len() != 2 {
            return None;
        }
        let multi = match op.opfuncid {
            F_JSONB_OBJECT_FIELD_TEXT if first => {
                terminal_text = true;
                false
            }
            F_JSONB_EXTRACT_PATH_TEXT if first => {
                terminal_text = true;
                true
            }
            F_JSONB_OBJECT_FIELD => false,
            F_JSONB_EXTRACT_PATH => true,
            _ => return None,
        };
        first = false;
        let c = op.args.nth(1).as_const()?;
        if c.constisnull {
            return None;
        }
        if multi {
            if c.consttype != TEXTARRAYOID {
                return None;
            }
            let keys = deconstruct_text_array(run, c.constvalue).ok()??;
            segs_rev.extend(keys.into_iter().rev());
        } else {
            if c.consttype != TEXTOID {
                return None;
            }
            segs_rev.push(varlena_payload(c.constvalue)?);
        }
        cur = strip_relabel(op.args.nth(0));
    }
    let var = cur.as_var()?;
    if var.vartype != JSONBOID {
        return None;
    }
    Some(Chain { varno: var.varno, varattno: var.varattno, segs_rev, terminal_text })
}

fn deconstruct_text_array<'mcx>(
    run: &PlannerRun<'mcx>,
    value: Datum,
) -> PgResult<Option<std::vec::Vec<&'mcx [u8]>>> {
    let img = crate::selfuncs::varlena_image_any(run.mcx, value)?;
    if arrayfuncs::arr_elemtype(img) != TEXTOID {
        return Ok(None);
    }
    let (values, nulls) = arrayfuncs::deconstruct_array(run.mcx, img, -1, false, b'i', true)?;
    let mut out = ::mcx::stdvec::try_vec(values.len(), "jsonb_stats")?;
    for (i, &v) in values.iter().enumerate() {
        if nulls[i] {
            return Ok(None);
        }
        let Some(p) = varlena_payload(v) else { return Ok(None) };
        out.push(p);
    }
    Ok(Some(out))
}

thread_local! {
    /// Keyed by the blob's content hash too, so a new ANALYZE never meets a stale index.
    static INDEX_CACHE: RefCell<HashMap<(Oid, i16, bool, u64), Rc<[u32]>>> =
        RefCell::new(HashMap::new());
}
const INDEX_CACHE_CAP: usize = 256;

struct Blob<'mcx> {
    bytes: &'mcx [u8],
    hdr: fmt::Header,
    offs: Rc<[u32]>,
}

impl<'mcx> Blob<'mcx> {
    fn find(&self, key: &[u8]) -> Option<fmt::Record<'mcx>> {
        fmt::find(self.bytes, &self.offs, key)
    }

    fn per_doc(&self) -> f64 {
        let h = &self.hdr;
        (h.nonnull_rows as f64 / h.samplerows.max(1) as f64) / h.analyzed_rows.max(1) as f64
    }

    fn untracked_presence(&self) -> f64 {
        if self.hdr.flags & fmt::FLAG_COMPLETE != 0 {
            return 0.0;
        }
        (self.hdr.untracked_frac * self.hdr.analyzed_rows as f64 * self.per_doc()).clamp(0.0, 1.0)
    }
}

fn lookup_blob<'mcx>(
    run: &PlannerRun<'mcx>,
    varno: i32,
    varattno: i16,
) -> PgResult<Option<Blob<'mcx>>> {
    let rte = rte_at(run, &run.root, varno as usize);
    let Some(base) = get_att_stats(run, rte.relid, varattno, rte.inh)? else {
        return Ok(None);
    };
    let Some(slot) = base.slots.iter().find(|s| s.kind == fmt::STATISTIC_KIND_JSONB_PATHS)
    else {
        return Ok(None);
    };
    let vals = slot.values()?;
    if vals.len() != 1 {
        return Ok(None);
    }
    let Some(bytes) = varlena_payload(vals[0]) else { return Ok(None) };
    let Some(hdr) = fmt::header(bytes) else { return Ok(None) };
    if hdr.analyzed_rows == 0 || hdr.samplerows == 0 {
        return Ok(None);
    }
    let key = (rte.relid, varattno, rte.inh, hdr.generation);
    let cached = INDEX_CACHE.with(|c| c.borrow().get(&key).cloned());
    let offs = match cached {
        Some(o) => o,
        None => {
            let Some(o) = fmt::index(bytes) else { return Ok(None) };
            let o: Rc<[u32]> = o.into();
            INDEX_CACHE.with(|c| {
                let mut c = c.borrow_mut();
                if c.len() >= INDEX_CACHE_CAP {
                    c.clear();
                }
                c.insert(key, o.clone());
            });
            o
        }
    };
    Ok(Some(Blob { bytes, hdr, offs }))
}

#[derive(Clone, Copy, PartialEq)]
enum CastClass {
    Number,
    /// the byte-order histogram, kept only if it stays sorted after conversion
    Temporal,
    Uuid,
}

fn cast_class(typid: Oid) -> Option<CastClass> {
    match typid {
        21 | 23 | 20 | 700 | 701 | 1700 => Some(CastClass::Number),
        1082 | 1114 | 1184 => Some(CastClass::Temporal),
        2950 => Some(CastClass::Uuid),
        _ => None,
    }
}

pub(crate) fn examine_jsonb_path_stats<'mcx>(
    run: &PlannerRun<'mcx>,
    vardata: &mut VariableStatData<'mcx>,
    varno: i32,
    node: Node<'mcx>,
) -> PgResult<()> {
    let mut cast: Option<(Oid, CastClass)> = None;
    let mut basenode = node;
    if let Some(cio) = node.as_coerce_via_io() {
        let Some(class) = cast_class(cio.resulttype) else { return Ok(()) };
        cast = Some((cio.resulttype, class));
        basenode = strip_relabel(cio.arg);
    }
    let Some(chain) = resolve_chain(run, basenode) else { return Ok(()) };
    if chain.varno != varno || chain.segs_rev.is_empty() {
        return Ok(());
    }
    if cast.is_some() && !chain.terminal_text {
        return Ok(());
    }
    let Some(blob) = lookup_blob(run, chain.varno, chain.varattno)? else { return Ok(()) };
    if may_index_array(&blob, &chain) {
        return Ok(());
    }
    let key = fmt::encode_path(chain.segs_rev.iter().rev().copied());
    let per_doc = blob.per_doc();
    let bundle = match blob.find(&key) {
        Some(r) => {
            let c = &r.counts;
            let nonnull_docs =
                if chain.terminal_text { c.present.saturating_sub(c.json_null) } else { c.present };
            let stanullfrac = (1.0 - nonnull_docs as f64 * per_doc).clamp(0.0, 1.0) as f32;
            let mut slots: PgVec<'mcx, PgStatisticSlotData<'mcx>> = PgVec::new_in(run.mcx);
            match cast {
                Some((typid, class)) => {
                    if let Some(s) = typed_mcv_slot(run, typid, &r, per_doc)? {
                        slots.push(s);
                    }
                    let usable = match class {
                        CastClass::Number => r.hist_kind == fmt::HIST_NUMERIC,
                        CastClass::Temporal => r.hist_kind == fmt::HIST_BYTES,
                        CastClass::Uuid => false,
                    };
                    if usable {
                        if let Some(s) = typed_hist_slot(run, typid, &r)? {
                            slots.push(s);
                        }
                    }
                }
                None if chain.terminal_text && r.nmcv() > 0 => {
                    let (values, numbers, image) = text_mcv(run, &r, per_doc)?;
                    slots.push(PgStatisticSlotData::from_decoded(
                        STATISTIC_KIND_MCV,
                        TEXT_EQ_OPERATOR,
                        DEFAULT_COLLATION_OID,
                        TEXTOID,
                        values,
                        numbers,
                        image,
                    ));
                }
                None => {}
            }
            PgStatisticBundle {
                stanullfrac,
                stawidth: r.avgwidth.max(4),
                stadistinct: r.ndistinct,
                slots,
            }
        }
        None => PgStatisticBundle {
            stanullfrac: (1.0 - blob.untracked_presence()) as f32,
            stawidth: 4,
            stadistinct: 0.0,
            slots: PgVec::new_in(run.mcx),
        },
    };
    vardata.stats = Some(leak_bundle(run.mcx, bundle)?);
    // C's expression-statistics rule: the expression's own columns.
    vardata.acl_ok = all_rows_selectable(run, &run.root, varno, Some(&[chain.varattno]))?;
    Ok(())
}

fn text_mcv<'mcx>(
    run: &PlannerRun<'mcx>,
    r: &fmt::Record<'_>,
    per_doc: f64,
) -> PgResult<(PgVec<'mcx, Datum>, PgVec<'mcx, f32>, PgVec<'mcx, u8>)> {
    let total: usize = r.mcvs().map(|(_, v)| (4 + v.len() + 7) & !7usize).sum();
    let mut image: PgVec<'mcx, u8> = mcx::vec_with_capacity_in(run.mcx, total)?;
    let mut offs: std::vec::Vec<usize> = ::mcx::stdvec::try_vec(r.nmcv(), "jsonb_stats")?;
    let mut numbers: PgVec<'mcx, f32> = mcx::vec_with_capacity_in(run.mcx, r.nmcv())?;
    for (cnt, v) in r.mcvs() {
        offs.push(image.len());
        mcx::vec_append_bytes(&mut image, &datum::set_varsize_4b(v.len() + datum::VARHDRSZ))?;
        mcx::vec_append_bytes(&mut image, v)?;
        while image.len() % 8 != 0 {
            mcx::vec_append_bytes(&mut image, &[0u8])?;
        }
        numbers.push((cnt as f64 * per_doc) as f32);
    }
    debug_assert!(image.len() <= total);
    let base = image.as_ptr() as usize;
    let mut values: PgVec<'mcx, Datum> = mcx::vec_with_capacity_in(run.mcx, offs.len())?;
    for o in offs {
        values.push(Datum::from_usize(base + o));
    }
    Ok((values, numbers, image))
}

struct Converter {
    typid: Oid,
    flinfo: types_fmgr::FmgrInfo,
    typioparam: Oid,
    typlen: i16,
    typbyval: bool,
}

impl Converter {
    fn new(typid: Oid) -> PgResult<Converter> {
        let (typinput, typioparam) = lsyscache::getTypeInputInfo(typid)?;
        let flinfo = fmgr_core::fmgr_info(typinput)?;
        let (typlen, typbyval) = lsyscache::get_typlenbyval(typid)?;
        Ok(Converter { typid, flinfo, typioparam, typlen, typbyval })
    }

    /// By-reference results are copied into `image` (offset returned; callers rebase).
    fn convert<'mcx>(
        &mut self,
        run: &PlannerRun<'mcx>,
        text: &[u8],
        image: &mut PgVec<'mcx, u8>,
    ) -> PgResult<Option<Datum>> {
        let Ok(cstr) = std::ffi::CString::new(text) else { return Ok(None) };
        let mut esn = types_fmgr::ErrorSaveNode::new(false);
        let mut d = Datum::null();
        let ok = types_fmgr::input_function_call_safe(
            &mut self.flinfo,
            Some(&cstr),
            self.typioparam,
            -1,
            run.mcx,
            Some(&mut esn),
            &mut d,
        )?;
        if !ok {
            return Ok(None);
        }
        if self.typbyval {
            return Ok(Some(d));
        }
        let p = d.as_usize() as *const u8;
        let len = match self.typlen {
            n if n > 0 => n as usize,
            -1 => adt_rangetypes::varsize_4b(p),
            _ => return Ok(None),
        };
        // SAFETY: `len` bytes at `p` are the live result (kept by flinfo).
        let bytes = unsafe { core::slice::from_raw_parts(p, len) };
        let off = image.len();
        mcx::vec_append_bytes(image, bytes)?;
        while image.len() % 8 != 0 {
            mcx::vec_append_bytes(image, &[0u8])?;
        }
        Ok(Some(Datum::from_usize(off)))
    }

    fn rebase<'mcx>(&self, staged: &mut [Datum], image: &PgVec<'mcx, u8>) {
        if self.typbyval {
            return;
        }
        let base = image.as_ptr() as usize;
        for d in staged.iter_mut() {
            *d = Datum::from_usize(base + d.as_usize());
        }
    }
}

fn type_ops(typid: Oid) -> PgResult<(Oid, Oid)> {
    let tc = typcache::lookup_type_cache(
        typid,
        typcache::TYPECACHE_EQ_OPR | typcache::TYPECACHE_LT_OPR,
    )?;
    Ok((tc.eq_opr(), tc.lt_opr()))
}

fn typed_mcv_slot<'mcx>(
    run: &PlannerRun<'mcx>,
    typid: Oid,
    r: &fmt::Record<'_>,
    per_doc: f64,
) -> PgResult<Option<PgStatisticSlotData<'mcx>>> {
    if r.nmcv() == 0 {
        return Ok(None);
    }
    let (eq_op, _) = type_ops(typid)?;
    if eq_op == types_core::InvalidOid {
        return Ok(None);
    }
    let mut conv = Converter::new(typid)?;
    let mut image: PgVec<'mcx, u8> = PgVec::new_in(run.mcx);
    let mut staged: std::vec::Vec<Datum> = ::mcx::stdvec::try_vec(r.nmcv(), "jsonb_stats")?;
    let mut numbers: PgVec<'mcx, f32> = mcx::vec_with_capacity_in(run.mcx, r.nmcv())?;
    for (cnt, v) in r.mcvs() {
        if let Some(d) = conv.convert(run, v, &mut image)? {
            staged.push(d);
            numbers.push((cnt as f64 * per_doc) as f32);
        }
    }
    if staged.is_empty() {
        return Ok(None);
    }
    conv.rebase(&mut staged, &image);
    let mut values: PgVec<'mcx, Datum> = mcx::vec_with_capacity_in(run.mcx, staged.len())?;
    for d in staged {
        values.push(d);
    }
    Ok(Some(PgStatisticSlotData::from_decoded(
        STATISTIC_KIND_MCV,
        eq_op,
        types_core::InvalidOid,
        conv.typid,
        values,
        numbers,
        image,
    )))
}

/// Every bound must convert, and a by-value histogram must stay sorted after conversion.
fn typed_hist_slot<'mcx>(
    run: &PlannerRun<'mcx>,
    typid: Oid,
    r: &fmt::Record<'_>,
) -> PgResult<Option<PgStatisticSlotData<'mcx>>> {
    if r.nbounds() < 2 {
        return Ok(None);
    }
    let (_, lt_op) = type_ops(typid)?;
    if lt_op == types_core::InvalidOid {
        return Ok(None);
    }
    let mut conv = Converter::new(typid)?;
    let mut image: PgVec<'mcx, u8> = PgVec::new_in(run.mcx);
    let mut staged: std::vec::Vec<Datum> = ::mcx::stdvec::try_vec(r.nbounds(), "jsonb_stats")?;
    for v in r.bounds() {
        match conv.convert(run, v, &mut image)? {
            Some(d) => staged.push(d),
            None => return Ok(None),
        }
    }
    if matches!(typid, 1082) {
        if staged.windows(2).any(|w| w[0].as_i32() > w[1].as_i32()) {
            return Ok(None);
        }
    } else if matches!(typid, 1114 | 1184)
        && staged.windows(2).any(|w| w[0].as_i64() > w[1].as_i64())
    {
        return Ok(None);
    }
    conv.rebase(&mut staged, &image);
    let mut values: PgVec<'mcx, Datum> = mcx::vec_with_capacity_in(run.mcx, staged.len())?;
    for d in staged {
        values.push(d);
    }
    Ok(Some(PgStatisticSlotData::from_decoded(
        STATISTIC_KIND_HISTOGRAM,
        lt_op,
        types_core::InvalidOid,
        conv.typid,
        values,
        PgVec::new_in(run.mcx),
        image,
    )))
}

/// None (C's matchingsel runs) unless the chain's own value is never an array or a string, whose `?`
/// matches elements the slot does not count.
pub(crate) fn try_jsonb_matching_sel<'mcx>(
    run: &PlannerRun<'mcx>,
    opfuncid: Oid,
    args: &[NodeId],
    varrelid: i32,
) -> PgResult<Option<f64>> {
    if args.len() != 2 {
        return Ok(None);
    }
    let any = match opfuncid {
        F_JSONB_EXISTS | F_JSONB_EXISTS_ALL => false,
        F_JSONB_EXISTS_ANY => true,
        _ => return Ok(None),
    };
    let lhs = strip_relabel(*run.root.expr_node(args[0]));
    let Some(chain) = resolve_chain(run, lhs) else { return Ok(None) };
    if chain.terminal_text || (varrelid != 0 && chain.varno != varrelid) {
        return Ok(None);
    }
    let Some(c) = run.root.expr_node(args[1]).as_const() else { return Ok(None) };
    if c.constisnull {
        return Ok(None);
    }
    let keys: std::vec::Vec<&[u8]> = if opfuncid == F_JSONB_EXISTS {
        if c.consttype != TEXTOID {
            return Ok(None);
        }
        let Some(k) = varlena_payload(c.constvalue) else { return Ok(None) };
        std::vec::Vec::from([k])
    } else {
        if c.consttype != TEXTARRAYOID {
            return Ok(None);
        }
        match deconstruct_text_array(run, c.constvalue)? {
            Some(ks) if !ks.is_empty() => ks,
            _ => return Ok(None),
        }
    };
    let Some(blob) = lookup_blob(run, chain.varno, chain.varattno)? else { return Ok(None) };
    if may_index_array(&blob, &chain) {
        return Ok(None);
    }
    let own = fmt::encode_path(chain.segs_rev.iter().rev().copied());
    match blob.find(&own) {
        Some(r) if r.counts.array > 0 || r.counts.string > 0 => return Ok(None),
        _ => {}
    }
    let per_doc = blob.per_doc();
    let untracked = blob.untracked_presence();
    let freqs = keys.iter().map(|k| {
        let key = fmt::encode_path(chain.segs_rev.iter().rev().copied().chain([*k]));
        blob.find(&key)
            .map_or(untracked, |r| (r.counts.present as f64 * per_doc).clamp(0.0, 1.0))
    });
    let sel = if any {
        1.0 - freqs.fold(1.0, |a, f| a * (1.0 - f))
    } else {
        freqs.fold(1.0, |a, f| a * f)
    };
    Ok(Some(sel.clamp(0.0, 1.0)))
}

fn may_index_array(blob: &Blob<'_>, chain: &Chain<'_>) -> bool {
    let mut prefix: std::vec::Vec<&[u8]> = std::vec::Vec::new();
    for &seg in chain.segs_rev.iter().rev() {
        if looks_like_index(seg) {
            let key = fmt::encode_path(prefix.iter().copied());
            if blob.find(&key).is_some_and(|r| r.counts.array > 0) {
                return true;
            }
        }
        prefix.push(seg);
    }
    false
}

/// C's strtoint(seg, 10) consumes the whole segment: white space, a sign, digits.
fn looks_like_index(seg: &[u8]) -> bool {
    let start = seg
        .iter()
        .position(|&b| !matches!(b, b' ' | b'\t' | b'\n' | 0x0b | 0x0c | b'\r'))
        .unwrap_or(seg.len());
    let s = &seg[start..];
    let s = s.strip_prefix(b"-").or_else(|| s.strip_prefix(b"+")).unwrap_or(s);
    !s.is_empty() && s.iter().all(u8::is_ascii_digit)
}

fn varlena_payload<'a>(d: Datum) -> Option<&'a [u8]> {
    let p = d.as_usize() as *const u8;
    if p.is_null() {
        return None;
    }
    // SAFETY: live varlena datum readable through its header byte.
    unsafe {
        let b0 = *p;
        if b0 & 0x01 != 0 {
            if b0 == 0x01 {
                return None; // toast pointer
            }
            let len = ((b0 as usize) >> 1) & 0x7F;
            Some(core::slice::from_raw_parts(p.add(1), len - 1))
        } else {
            if b0 & 0x03 != 0 {
                return None; // compressed
            }
            let len = adt_rangetypes::varsize_4b(p);
            Some(core::slice::from_raw_parts(p.add(4), len - 4))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::looks_like_index;

    #[test]
    fn index_segments_follow_strtoint() {
        for s in ["0", "1", "-1", "+2", " 3", "\t\n42", "007"] {
            assert!(looks_like_index(s.as_bytes()), "{s:?}");
        }
        for s in ["", "-", "a", "1a", "1 ", "1.0", "0x1", "tags"] {
            assert!(!looks_like_index(s.as_bytes()), "{s:?}");
        }
    }
}
