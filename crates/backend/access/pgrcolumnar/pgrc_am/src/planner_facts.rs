//! Columnar planner facts for a relation WITHOUT parts (plan capture on a
//! machine without the data): two SQL faces over the footer fold and the
//! process-grain fact cache.
//!
//! - `pgrc_publish_planner_facts(regclass, text) → bool` parses one JSON
//!   document into a [`FooterFacts`] and publishes it under the relation's
//!   CURRENT fact-cache key ([`crate::footer::publish_facts`]): the
//!   manifest's `(gen, publisher_fxid)` when the table has parts, the
//!   sentinel `(0, 0)` when it has none. The ordinary seams (`footer_size`,
//!   `footer_ndv`, `footer_col_bytes`, `footer_avg_widths`) serve it and
//!   relcache invalidation evicts it exactly like a real fold.
//! - `pgrc_dump_planner_facts(regclass) → text` emits the same JSON from
//!   the deep fold of a relation that has parts (the remote dump).
//! - `pgrc_planner_sorted_attnos(regclass) → text` answers the order the
//!   planner may claim over a bare scan (`reloptions::planner_sorted_attnos`)
//!   as comma-separated attnos: the Gravity plan capture's `-- rel … sorted=`
//!   fact (`scripts/gravity-capture-plans.sh`; decompose.md §4.8).
//!
//! JSON, one object: `{"rows": N, "bytes": B, "granules": G, "parts": P, "sorted":
//! [attno, …], "cols": [{"attno": 1, "name": "id", "bytes": B1,
//! "ndv": D1, "avg_width": W1, "zone": true, "bloom": false, "dict":
//! false, "max_len": L1, "codec": "word", "ncodes": 0}, …], "zones":
//! {"7": [[min, max], …], …}}`. `cols` is dense by attno (every column, in
//! order); relation-grain `bytes` defaults to Σ column bytes; `name` is
//! checked against the attribute name when present; the three booleans (the
//! qual evaluation classes, default false) map to
//! `ColCostFacts::qual_class`; `max_len` (default 0 = unknown) to
//! `ColCostFacts::max_len`; `codec` (`word` / `dict` / `text` /
//! `fixed`, default unknown) and `ncodes` (default 0) to
//! `ColCostFacts::{codec, ncodes}`; `nonnull` (NEW-stats-2, default
//! unknown) to `ColCostFacts::nonnull`; `granules` to `FooterFacts::granules`;
//! `zones` (one `[min, max]` pair per granule in granule order, keyed
//! by attno, `granules` pairs each) to `FooterFacts::zones`. `sorted` is
//! validated and carried by the file but has no field yet.
//!
//! Registration follows the pgrust-native precedent (`execmain::
//! SQE_BUILTINS`, reserved-range oids): the rows live in
//! [`PLANNER_FACTS_BUILTINS`], installed by `seams_init`, and the pg_proc
//! rows are created on demand (`CREATE FUNCTION ... LANGUAGE internal`)
//! — the shipped catalog carries no delta.

use datum::Datum;
use types_core::Oid;
use types_error::{PgError, PgResult};
use types_fmgr::{varlena_result, FmgrBuiltin, FmgrInfo, FunctionCallInfoBaseData as Fcinfo};
use types_rel::Relation;
use types_storage::lock::AccessShareLock;

use crate::footer::{
    ColCostFacts, FooterFacts, ZoneFacts, CODEC_DICT, CODEC_FIXED, CODEC_TEXT, CODEC_UNKNOWN, CODEC_WORD,
    QUAL_CLASS_BLOOM, QUAL_CLASS_DICT, QUAL_CLASS_ZONE,
};

/// The JSON spelling of each `CODEC_*` kind (unknown is omitted on dump).
const CODEC_NAMES: [(u8, &str); 4] =
    [(CODEC_WORD, "word"), (CODEC_DICT, "dict"), (CODEC_TEXT, "text"), (CODEC_FIXED, "fixed")];

// The Gravity pgrc faces take 9060..=9064 of the pgrust reserved range (9000..=9099); 9020 / 9021 are the buffer
// pool's (bufmgr pool_sql.rs). extra_builtin answers the first row with an oid, so the oids must stay distinct
// across seams_init's EXTRA_BUILTINS (D3128).
pub const PGRC_PUBLISH_PLANNER_FACTS_FOID: Oid = 9060;
pub const PGRC_DUMP_PLANNER_FACTS_FOID: Oid = 9061;
pub const PGRC_PLANNER_SORTED_ATTNOS_FOID: Oid = 9062;

pub const PLANNER_FACTS_BUILTINS: &[FmgrBuiltin] = &[
    FmgrBuiltin {
        foid: PGRC_PUBLISH_PLANNER_FACTS_FOID,
        name: "pgrc_publish_planner_facts",
        nargs: 2,
        strict: true,
        retset: false,
        func: fc_pgrc_publish_planner_facts,
    },
    FmgrBuiltin {
        foid: PGRC_DUMP_PLANNER_FACTS_FOID,
        name: "pgrc_dump_planner_facts",
        nargs: 1,
        strict: true,
        retset: false,
        func: fc_pgrc_dump_planner_facts,
    },
    FmgrBuiltin {
        foid: PGRC_PLANNER_SORTED_ATTNOS_FOID,
        name: "pgrc_planner_sorted_attnos",
        nargs: 1,
        strict: true,
        retset: false,
        func: fc_pgrc_planner_sorted_attnos,
    },
    FmgrBuiltin {
        foid: crate::layout::PGRC_PART_LAYOUT_FOID,
        name: "pgrc_part_layout",
        nargs: 1,
        strict: true,
        retset: false,
        func: fc_pgrc_part_layout,
    },
    FmgrBuiltin {
        foid: crate::layout::PGRC_PART_STREAMS_FOID,
        name: "pgrc_part_streams",
        nargs: 1,
        strict: true,
        retset: false,
        func: fc_pgrc_part_streams,
    },
];

/// `pgrc_part_layout(regclass) → text`: [`crate::layout::part_layout`] (sortpath's debug face).
pub fn fc_pgrc_part_layout(_flinfo: Option<&mut FmgrInfo>, fcinfo: &mut Fcinfo) -> PgResult<Datum> {
    let mcx = fcinfo.result_mcx();
    let relid = fcinfo.arg_oid(0);
    let rel = open_pgrc(mcx, relid, "pgrc_part_layout")?;
    let outcome = crate::layout::part_layout(&rel);
    rel.close(AccessShareLock)?;
    let text = outcome?;
    let mut img: mcx::PgVec<'_, u8> = mcx::PgVec::new_in(mcx);
    img.extend_from_slice(&[0u8; datum::VARHDRSZ]);
    img.extend_from_slice(text.as_bytes());
    Ok(varlena_result(datum::Varlena::from_image(img)))
}

/// `pgrc_part_streams(regclass) → text`: [`crate::layout::part_streams`] (sortpath's layout diff).
pub fn fc_pgrc_part_streams(_flinfo: Option<&mut FmgrInfo>, fcinfo: &mut Fcinfo) -> PgResult<Datum> {
    let mcx = fcinfo.result_mcx();
    let relid = fcinfo.arg_oid(0);
    let rel = open_pgrc(mcx, relid, "pgrc_part_streams")?;
    let outcome = crate::layout::part_streams(&rel);
    rel.close(AccessShareLock)?;
    let text = outcome?;
    let mut img: mcx::PgVec<'_, u8> = mcx::PgVec::new_in(mcx);
    img.extend_from_slice(&[0u8; datum::VARHDRSZ]);
    img.extend_from_slice(text.as_bytes());
    Ok(varlena_result(datum::Varlena::from_image(img)))
}

fn err(msg: String) -> Box<PgError> {
    Box::new(PgError::error(msg)) // alloc: Cold
}

fn open_pgrc<'a>(mcx: mcx::Mcx<'a>, relid: Oid, face: &str) -> PgResult<Relation<'a>> {
    let rel = relation_seams::relation_open::call(mcx, relid, AccessShareLock)?;
    if !tableam_vocab::is_pgrcolumnar_am_oid(rel.rd_rel.relam) {
        let name = rel.name().to_string(); // alloc: Cold
        rel.close(AccessShareLock)?;
        return Err(err(format!("{face}: \"{name}\" is not a pgrcolumnar table"))); // alloc: Cold
    }
    Ok(rel)
}

pub fn fc_pgrc_publish_planner_facts(
    _flinfo: Option<&mut FmgrInfo>,
    fcinfo: &mut Fcinfo,
) -> PgResult<Datum> {
    let mcx = fcinfo.result_mcx();
    let relid = fcinfo.arg_oid(0);
    // SAFETY: strict function; arg 1 is a non-null text datum.
    let json = crate::alloc::try_to_vec(unsafe { fcinfo.arg_varlena_packed(1)? }.data(), "planner_facts")?;
    let json = std::str::from_utf8(&json)
        .map_err(|_| err("pgrc_publish_planner_facts: facts are not UTF-8".to_string()))?; // alloc: Cold
    let rel = open_pgrc(mcx, relid, "pgrc_publish_planner_facts")?;
    let outcome = facts_from_json(&rel, json).and_then(|f| crate::footer::publish_facts(&rel, f));
    rel.close(AccessShareLock)?;
    outcome?;
    Ok(Datum::from_bool(true))
}

pub fn fc_pgrc_dump_planner_facts(
    _flinfo: Option<&mut FmgrInfo>,
    fcinfo: &mut Fcinfo,
) -> PgResult<Datum> {
    let mcx = fcinfo.result_mcx();
    let relid = fcinfo.arg_oid(0);
    let rel = open_pgrc(mcx, relid, "pgrc_dump_planner_facts")?;
    let outcome = crate::footer::deep_facts(&rel).map(|facts| facts.map(|f| facts_to_json(&rel, &f)));
    let name = rel.name().to_string();
    rel.close(AccessShareLock)?;
    let Some(text) = outcome? else {
        return Err(err(format!("pgrc_dump_planner_facts: \"{name}\" has no committed parts"))); // alloc: Cold
    };
    let mut img: mcx::PgVec<'_, u8> = mcx::PgVec::new_in(mcx);
    img.extend_from_slice(&[0u8; datum::VARHDRSZ]);
    img.extend_from_slice(text.as_bytes());
    Ok(varlena_result(datum::Varlena::from_image(img)))
}

/// `pgrc_planner_sorted_attnos(regclass) → text`: the order the planner may claim over a bare scan of the
/// relation ([`crate::reloptions::planner_sorted_attnos`]: the declared `sorted_by` key, ASC NULLS LAST
/// lexicographic, when the table is no part or one part whose SortKey attests it), as comma-separated attnos — `''` when none. The
/// Gravity plan capture writes it into a fixture's `-- rel … sorted=` line; the live side
/// (`gravity_hosts::facts::RelcacheFacts`) calls the same function, so the two agree by construction.
pub fn fc_pgrc_planner_sorted_attnos(
    _flinfo: Option<&mut FmgrInfo>,
    fcinfo: &mut Fcinfo,
) -> PgResult<Datum> {
    let mcx = fcinfo.result_mcx();
    let relid = fcinfo.arg_oid(0);
    let rel = open_pgrc(mcx, relid, "pgrc_planner_sorted_attnos")?;
    let outcome = crate::reloptions::planner_sorted_attnos(&rel, None);
    rel.close(AccessShareLock)?;
    let attnos = outcome?;
    let mut names: Vec<String> = crate::alloc::try_vec(attnos.len(), "planner_facts")?;
    names.extend(attnos.iter().map(|a| a.to_string()));
    let text = names.join(",");
    let mut img: mcx::PgVec<'_, u8> = mcx::PgVec::new_in(mcx);
    img.extend_from_slice(&[0u8; datum::VARHDRSZ]);
    img.extend_from_slice(text.as_bytes());
    Ok(varlena_result(datum::Varlena::from_image(img)))
}

fn attname(rel: &Relation<'_>, i: usize) -> String {
    String::from_utf8_lossy(rel.rd_att.attrs[i].attname.name_str()).into_owned()
}

fn facts_from_json(rel: &Relation<'_>, text: &str) -> PgResult<FooterFacts> {
    const FACE: &str = "pgrc_publish_planner_facts";
    let bad = |m: String| err(format!("{FACE}: {m}")); // alloc: Cold
    let doc = parse_json(text).map_err(|m| bad(format!("bad JSON: {m}")))?; // alloc: Cold
    let obj = doc.obj().ok_or_else(|| bad("top level is not an object".to_string()))?; // alloc: Cold
    let rows = field_u64(obj, "rows").map_err(bad)?;
    let cols = field(obj, "cols")
        .and_then(Json::arr)
        .ok_or_else(|| bad("\"cols\" must be an array".to_string()))?; // alloc: Cold
    let natts = rel.rd_att.attrs.len();
    if cols.len() != natts {
        return Err(bad(format!("\"cols\" has {} entries, the relation has {natts} columns", cols.len()))); // alloc: Cold
    }
    let mut disk_bytes = crate::alloc::try_vec(natts, "planner_facts")?;
    let mut ndv = crate::alloc::try_vec(natts, "planner_facts")?;
    let mut avg_width = crate::alloc::try_vec(natts, "planner_facts")?;
    let mut qual_class = crate::alloc::try_vec(natts, "planner_facts")?;
    let mut max_len = crate::alloc::try_vec(natts, "planner_facts")?;
    let mut nonnull = crate::alloc::try_vec(natts, "planner_facts")?;
    let mut codec = crate::alloc::try_vec(natts, "planner_facts")?;
    let mut ncodes = crate::alloc::try_vec(natts, "planner_facts")?;
    for (i, c) in cols.iter().enumerate() {
        let o = c.obj().ok_or_else(|| bad(format!("cols[{i}] is not an object")))?; // alloc: Cold
        let attno = field_u64(o, "attno").map_err(bad)?;
        if attno != i as u64 + 1 {
            return Err(bad(format!("cols[{i}] has attno {attno}, expected {}", i + 1))); // alloc: Cold
        }
        if let Some(name) = field(o, "name").and_then(Json::str) {
            let att = attname(rel, i);
            if !name.eq_ignore_ascii_case(&att) {
                return Err(bad(format!("cols[{i}] is named \"{name}\", the column is \"{att}\""))); // alloc: Cold
            }
        }
        disk_bytes.push(field_u64(o, "bytes").map_err(bad)?);
        ndv.push(field_u64(o, "ndv").map_err(bad)?);
        let w = optional_u64(o, "avg_width").map_err(bad)?.unwrap_or(0);
        let avg = i32::try_from(w).map_err(|_| bad(format!("cols[{i}].avg_width {w} out of range")))?; // alloc: Cold
        avg_width.push(avg);
        let mut bits = 0u8;
        for (key, bit) in [("zone", QUAL_CLASS_ZONE), ("bloom", QUAL_CLASS_BLOOM), ("dict", QUAL_CLASS_DICT)] {
            if optional_bool(o, key).map_err(bad)? {
                bits |= bit;
            }
        }
        qual_class.push(bits);
        let l = optional_u64(o, "max_len").map_err(bad)?.unwrap_or(0);
        let mlen = i32::try_from(l).map_err(|_| bad(format!("cols[{i}].max_len {l} out of range")))?; // alloc: Cold
        max_len.push(mlen);
        nonnull.push(optional_u64(o, "nonnull").map_err(bad)?);
        codec.push(match field(o, "codec") {
            None | Some(Json::Null) => CODEC_UNKNOWN,
            Some(Json::Str(s)) => CODEC_NAMES
                .iter()
                .find(|(_, n)| *n == s)
                .map(|(k, _)| *k)
                .ok_or_else(|| bad(format!("cols[{i}].codec \"{s}\" is not word/dict/text/fixed")))?, // alloc: Cold
            Some(_) => return Err(bad(format!("cols[{i}].codec must be a string"))), // alloc: Cold
        });
        ncodes.push(optional_u64(o, "ncodes").map_err(bad)?.unwrap_or(0));
    }
    let bytes = optional_u64(obj, "bytes").map_err(bad)?.unwrap_or_else(|| disk_bytes.iter().sum());
    let granules = optional_u64(obj, "granules").map_err(bad)?.unwrap_or(0);
    let parts = optional_u64(obj, "parts").map_err(bad)?.unwrap_or(crate::footer::PARTS_UNKNOWN);
    if let Some(sorted) = field(obj, "sorted") {
        let list = sorted.arr().ok_or_else(|| bad("\"sorted\" must be an array".to_string()))?; // alloc: Cold
        for s in list {
            match s.u64() {
                Some(a) if (1..=natts as u64).contains(&a) => {}
                _ => return Err(bad(format!("\"sorted\" entries must be attnos in 1..={natts}"))), // alloc: Cold
            }
        }
    }
    let mut zones = Vec::new();
    if let Some(z) = field(obj, "zones") {
        let map = z.obj().ok_or_else(|| bad("\"zones\" must be an object keyed by attno".to_string()))?; // alloc: Cold
        for (key, ranges) in map {
            let attno: u64 = key.parse().ok().filter(|a| (1..=natts as u64).contains(a)).ok_or_else(|| {
                bad(format!("\"zones\" key \"{key}\" is not an attno in 1..={natts}")) // alloc: Cold
            })?;
            let list = ranges.arr().ok_or_else(|| bad(format!("zones[{key}] must be an array")))?; // alloc: Cold
            if list.len() as u64 != granules {
                return Err(bad(format!("zones[{key}] has {} ranges, \"granules\" is {granules}", list.len()))); // alloc: Cold
            }
            let mut out = crate::alloc::try_vec(list.len(), "planner_facts")?;
            for r in list {
                let pair = r.arr().filter(|p| p.len() == 2).and_then(|p| Some((p[0].i64()?, p[1].i64()?)));
                let pair = pair.ok_or_else(|| bad(format!("zones[{key}] entries must be [min, max] integers")))?; // alloc: Cold
                out.push(pair);
            }
            zones.push(ZoneFacts::new(attno as u32, pgrc_format::zonemap::ZoneMap::encode(&out).map_err(crate::format_error)?)?); // alloc: Cold charged the catalog's (publish_facts: the generation's or the RelEntry's held_bytes)
        }
    }
    Ok(FooterFacts {
        gen: 0,
        rows,
        bytes,
        granules,
        parts,
        ndv: Some(ndv),
        cols: Some(ColCostFacts { disk_bytes, avg_width, qual_class, max_len, nonnull, codec, ncodes }),
        zones,
    })
}

fn facts_to_json(rel: &Relation<'_>, f: &FooterFacts) -> String {
    let natts = rel.rd_att.attrs.len();
    let mut out = format!(
        "{{\"rows\": {}, \"bytes\": {}, \"granules\": {}, \"parts\": {}, \"sorted\": [],\n \"cols\": [\n",
        f.rows, f.bytes, f.granules, f.parts
    );
    for i in 0..natts {
        let bytes = f.cols.as_ref().and_then(|c| c.disk_bytes.get(i)).copied().unwrap_or(0);
        let width = f.cols.as_ref().and_then(|c| c.avg_width.get(i)).copied().unwrap_or(0);
        let ndv = f.ndv.as_ref().and_then(|v| v.get(i)).copied().unwrap_or(0);
        let class = f.cols.as_ref().and_then(|c| c.qual_class.get(i)).copied().unwrap_or(0);
        let max_len = f.cols.as_ref().and_then(|c| c.max_len.get(i)).copied().unwrap_or(0);
        let codec = f.cols.as_ref().and_then(|c| c.codec.get(i)).copied().unwrap_or(CODEC_UNKNOWN);
        let ncodes = f.cols.as_ref().and_then(|c| c.ncodes.get(i)).copied().unwrap_or(0);
        let codec_field = CODEC_NAMES
            .iter()
            .find(|(k, _)| *k == codec)
            .map_or(String::new(), |(_, n)| format!(", \"codec\": \"{n}\""));
        out.push_str(&format!(
            "  {{\"attno\": {}, \"name\": {}, \"bytes\": {bytes}, \"ndv\": {ndv}, \"avg_width\": {width}, \
             \"zone\": {}, \"bloom\": {}, \"dict\": {}, \"max_len\": {max_len}{codec_field}, \"ncodes\": {ncodes}}}{}\n",
            i + 1,
            json_string(&attname(rel, i)),
            class & QUAL_CLASS_ZONE != 0,
            class & QUAL_CLASS_BLOOM != 0,
            class & QUAL_CLASS_DICT != 0,
            if i + 1 < natts { "," } else { "" }
        ));
    }
    out.push_str(" ],\n \"zones\": {");
    for (j, z) in f.zones.iter().enumerate() {
        out.push_str(&format!("{}\n  \"{}\": [", if j > 0 { "," } else { "" }, z.attno));
        for (g, (lo, hi)) in z.ranges().iter().enumerate() {
            out.push_str(&format!("{}[{lo},{hi}]", if g > 0 { "," } else { "" }));
        }
        out.push(']');
    }
    out.push_str("\n }}\n");
    out
}

fn json_string(s: &str) -> String {
    let mut out = String::new();
    out.push('"');
    for ch in s.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

// ---- a minimal JSON reader (the facts file is small and fixed-shape) ----

#[derive(Debug)]
enum Json {
    Null,
    Bool(bool),
    /// An integer literal that fits i64, kept exact (zone keys reach 2^63).
    Int(i64),
    Num(f64),
    Str(String),
    Arr(Vec<Json>),
    Obj(Vec<(String, Json)>),
}

impl Json {
    fn obj(&self) -> Option<&[(String, Json)]> {
        match self {
            Json::Obj(v) => Some(v),
            _ => None,
        }
    }
    fn arr(&self) -> Option<&[Json]> {
        match self {
            Json::Arr(v) => Some(v),
            _ => None,
        }
    }
    fn str(&self) -> Option<&str> {
        match self {
            Json::Str(s) => Some(s),
            _ => None,
        }
    }
    fn u64(&self) -> Option<u64> {
        match self {
            Json::Int(i) => u64::try_from(*i).ok(),
            Json::Num(n) if *n >= 0.0 && *n <= u64::MAX as f64 && n.fract() == 0.0 => Some(*n as u64),
            _ => None,
        }
    }
    fn i64(&self) -> Option<i64> {
        match self {
            Json::Int(i) => Some(*i),
            _ => None,
        }
    }
}

fn field<'a>(obj: &'a [(String, Json)], key: &str) -> Option<&'a Json> {
    obj.iter().find(|(k, _)| k == key).map(|(_, v)| v)
}

fn field_u64(obj: &[(String, Json)], key: &str) -> Result<u64, String> {
    optional_u64(obj, key)?.ok_or_else(|| format!("\"{key}\" is required")) // alloc: Cold
}

fn optional_bool(obj: &[(String, Json)], key: &str) -> Result<bool, String> {
    match field(obj, key) {
        None | Some(Json::Null) => Ok(false),
        Some(Json::Bool(b)) => Ok(*b),
        Some(_) => Err(format!("\"{key}\" must be a boolean")), // alloc: Cold
    }
}

fn optional_u64(obj: &[(String, Json)], key: &str) -> Result<Option<u64>, String> {
    match field(obj, key) {
        None | Some(Json::Null) => Ok(None),
        Some(v) => v.u64().map(Some).ok_or_else(|| format!("\"{key}\" must be a non-negative integer")), // alloc: Cold
    }
}

fn parse_json(text: &str) -> Result<Json, String> {
    let mut p = Reader { b: text.as_bytes(), i: 0 };
    let v = p.value()?;
    p.ws();
    if p.i != p.b.len() {
        return Err(format!("trailing characters at byte {}", p.i)); // alloc: Cold
    }
    Ok(v)
}

struct Reader<'a> {
    b: &'a [u8],
    i: usize,
}

impl Reader<'_> {
    fn ws(&mut self) {
        while self.i < self.b.len() && matches!(self.b[self.i], b' ' | b'\t' | b'\n' | b'\r') {
            self.i += 1;
        }
    }

    fn peek(&self) -> Option<u8> {
        self.b.get(self.i).copied()
    }

    fn expect(&mut self, c: u8) -> Result<(), String> {
        if self.peek() == Some(c) {
            self.i += 1;
            Ok(())
        } else {
            Err(format!("expected '{}' at byte {}", c as char, self.i)) // alloc: Cold
        }
    }

    fn literal(&mut self, word: &str, v: Json) -> Result<Json, String> {
        if self.b[self.i..].starts_with(word.as_bytes()) {
            self.i += word.len();
            Ok(v)
        } else {
            Err(format!("bad literal at byte {}", self.i)) // alloc: Cold
        }
    }

    fn value(&mut self) -> Result<Json, String> {
        self.ws();
        match self.peek() {
            None => Err("unexpected end of input".to_string()), // alloc: Cold
            Some(b'{') => self.object(),
            Some(b'[') => self.array(),
            Some(b'"') => self.string().map(Json::Str),
            Some(b't') => self.literal("true", Json::Bool(true)),
            Some(b'f') => self.literal("false", Json::Bool(false)),
            Some(b'n') => self.literal("null", Json::Null),
            Some(c) if c == b'-' || c.is_ascii_digit() => self.number(),
            Some(c) => Err(format!("unexpected '{}' at byte {}", c as char, self.i)), // alloc: Cold
        }
    }

    fn object(&mut self) -> Result<Json, String> {
        self.expect(b'{')?;
        let mut out = Vec::new();
        self.ws();
        if self.peek() == Some(b'}') {
            self.i += 1;
            return Ok(Json::Obj(out));
        }
        loop {
            self.ws();
            let k = self.string()?;
            self.ws();
            self.expect(b':')?;
            let v = self.value()?;
            out.push((k, v));
            self.ws();
            match self.peek() {
                Some(b',') => self.i += 1,
                Some(b'}') => {
                    self.i += 1;
                    return Ok(Json::Obj(out));
                }
                _ => return Err(format!("expected ',' or '}}' at byte {}", self.i)), // alloc: Cold
            }
        }
    }

    fn array(&mut self) -> Result<Json, String> {
        self.expect(b'[')?;
        let mut out = Vec::new();
        self.ws();
        if self.peek() == Some(b']') {
            self.i += 1;
            return Ok(Json::Arr(out));
        }
        loop {
            out.push(self.value()?);
            self.ws();
            match self.peek() {
                Some(b',') => self.i += 1,
                Some(b']') => {
                    self.i += 1;
                    return Ok(Json::Arr(out));
                }
                _ => return Err(format!("expected ',' or ']' at byte {}", self.i)), // alloc: Cold
            }
        }
    }

    fn string(&mut self) -> Result<String, String> {
        self.expect(b'"')?;
        let mut out = Vec::new();
        loop {
            let Some(c) = self.peek() else { return Err("unterminated string".to_string()) }; // alloc: Cold
            self.i += 1;
            match c {
                b'"' => break,
                b'\\' => {
                    let Some(e) = self.peek() else { return Err("unterminated escape".to_string()) }; // alloc: Cold
                    self.i += 1;
                    match e {
                        b'"' | b'\\' | b'/' => out.push(e),
                        b'b' => out.push(8),
                        b'f' => out.push(12),
                        b'n' => out.push(b'\n'),
                        b'r' => out.push(b'\r'),
                        b't' => out.push(b'\t'),
                        b'u' => {
                            let hex = self.b.get(self.i..self.i + 4).ok_or_else(|| "short \\u escape".to_string())?; // alloc: Cold
                            let cp = u32::from_str_radix(std::str::from_utf8(hex).map_err(|_| "bad \\u escape".to_string())?, 16) // alloc: Cold
                                .map_err(|_| "bad \\u escape".to_string())?; // alloc: Cold
                            self.i += 4;
                            let ch = char::from_u32(cp).ok_or_else(|| "unsupported \\u escape (surrogate)".to_string())?; // alloc: Cold
                            let mut buf = [0u8; 4];
                            out.extend_from_slice(ch.encode_utf8(&mut buf).as_bytes());
                        }
                        _ => return Err(format!("bad escape at byte {}", self.i)), // alloc: Cold
                    }
                }
                _ => out.push(c),
            }
        }
        String::from_utf8(out).map_err(|_| "string is not UTF-8".to_string()) // alloc: Cold
    }

    fn number(&mut self) -> Result<Json, String> {
        let start = self.i;
        if self.peek() == Some(b'-') {
            self.i += 1;
        }
        while let Some(c) = self.peek() {
            if c.is_ascii_digit() || matches!(c, b'.' | b'e' | b'E' | b'+' | b'-') {
                self.i += 1;
            } else {
                break;
            }
        }
        let s = std::str::from_utf8(&self.b[start..self.i]).map_err(|_| "bad number".to_string())?; // alloc: Cold
        if let Ok(i) = s.parse::<i64>() {
            return Ok(Json::Int(i));
        }
        s.parse::<f64>().map(Json::Num).map_err(|_| format!("bad number '{s}' at byte {start}")) // alloc: Cold
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn json_reader_round_trips_the_facts_shape() {
        let doc = parse_json(
            r#"{"rows": 7, "granules": 2, "sorted": [1, 3],
                "cols": [{"attno": 1, "name": "a\"b", "bytes": 10, "ndv": 3, "avg_width": 8},
                         {"attno": 2, "bytes": 0, "ndv": 0}]}"#,
        )
        .unwrap();
        let obj = doc.obj().unwrap();
        assert_eq!(field_u64(obj, "rows").unwrap(), 7);
        assert_eq!(optional_u64(obj, "bytes").unwrap(), None);
        let cols = field(obj, "cols").unwrap().arr().unwrap();
        assert_eq!(cols.len(), 2);
        assert_eq!(field(cols[0].obj().unwrap(), "name").unwrap().str(), Some("a\"b"));
        assert_eq!(field(obj, "sorted").unwrap().arr().unwrap()[1].u64(), Some(3));
    }

    #[test]
    fn json_reader_refuses_malformed_input() {
        assert!(parse_json("{\"rows\": }").is_err());
        assert!(parse_json("[1, 2").is_err());
        assert!(parse_json("{} x").is_err());
        assert!(parse_json("{\"n\": -1}").unwrap().obj().unwrap()[0].1.u64().is_none());
        assert!(parse_json("{\"n\": 1.5}").unwrap().obj().unwrap()[0].1.u64().is_none());
    }

    #[test]
    fn json_string_escapes() {
        assert_eq!(json_string("a\"b\\c\n"), "\"a\\\"b\\\\c\\u000a\"");
    }

    /// Zone keys are read exactly up to i64's edge (an f64 would round
    /// 2^62 + 1 away); the codec names map to the `CODEC_*` kinds.
    #[test]
    fn json_reader_keeps_integers_exact() {
        let doc = parse_json(r#"{"zones": {"1": [[-4611686018427387905, 4611686018427387905]]}, "n": 1e3}"#).unwrap();
        let obj = doc.obj().unwrap();
        let z = field(obj, "zones").unwrap().obj().unwrap();
        let pair = z[0].1.arr().unwrap()[0].arr().unwrap();
        assert_eq!((pair[0].i64(), pair[1].i64()), (Some(-4611686018427387905), Some(4611686018427387905)));
        assert_eq!(field(obj, "n").unwrap().u64(), Some(1000));
        assert!(field(obj, "n").unwrap().i64().is_none(), "a float literal is not an exact integer");
        for (kind, name) in CODEC_NAMES {
            assert_eq!(CODEC_NAMES.iter().find(|(_, n)| *n == name).map(|(k, _)| *k), Some(kind));
        }
    }
}
