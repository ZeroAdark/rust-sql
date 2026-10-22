//! `pgrc_part_layout(regclass) → text` (sortpath, a debug face like the plan rig's in planner_facts.rs; created on demand,
//! `CREATE FUNCTION pgrc_part_layout(regclass) RETURNS text LANGUAGE internal STRICT VOLATILE AS 'pgrc_part_layout'`):
//! one line per committed part of the visible generation, `part <no> rows=<n> key=<key> paths=<path:kind,...>`, where
//! `key` is the part's SortKey section (`attno` for a column key, `attno->'a'->>'b'` for a path key, `-` when the part
//! is unordered) and `paths` its PathTable's shredded lanes at their sealed kind. The witness reads which parts carry a
//! declared key and which paths each part elected; nothing else consumes it.

use pgrc_format::part::{SectionKind, StreamRole};
use pgrc_format::shredlane::ShredLaneKind;
use pgrc_format::sortkey::{SortKeyRecord, SORT_KEY_KIND_TEXT_PATH};
use types_error::{PgError, PgResult};
use types_rel::Relation;

/// V5SYNC r14: trunk moved the debug faces to 9063 / 9064 (beside the planner facts 9060..=9062); the pgrc_front
/// builtins keep 9023..=9044.
pub const PGRC_PART_LAYOUT_FOID: types_core::Oid = 9063;
/// `pgrc_part_streams(regclass) → text` ([`part_streams`]).
pub const PGRC_PART_STREAMS_FOID: types_core::Oid = 9064;

fn err(msg: String) -> Box<PgError> {
    Box::new(PgError::error(msg)) // alloc: Cold
}

/// The stream lines of `rel` (sortpath's layout diff; empty when it has no committed publish): per committed part, one
/// line per stream, `part <no> att=<attno> path=<path or -> role=<role> class=<c> enc=<e> width=<w> fixed=<n>
/// bytes=<extent bytes> values=<n> extents=<n>`, so two tables' parts compare stream by stream (encodings, sizes).
pub fn part_streams(rel: &Relation<'_>) -> PgResult<String> {
    let mut out = String::new();
    each_part(rel, |rec_no, rows, part| {
        let paths = pgrc_read::read_path_table(part).map_err(crate::read_error)?;
        let dirx = part.stream_directory().map_err(crate::read_error)?;
        for st in dirx.streams() {
            let e = &st.entry;
            let path = match (e.path_ord, paths.as_ref()) {
                (0, _) => "-".to_string(),
                // a text result holds no NUL: the octet-length lane's reserved path prints as `\0octet_length`
                (o, Some(p)) => p.get(o as usize - 1).map_or_else(|| format!("#{o}"), |x| x.replace('\0', "\\0")),
                (o, None) => format!("#{o}"),
            };
            let bytes: u64 = st.extents.iter().map(|x| x.len).sum();
            out.push_str(&format!(
                "part {rec_no} rows={rows} att={} path={path} role={:?} class={} enc={} width={} fixed={} bytes={bytes} values={} extents={} wrapper={}\n",
                e.attno, st.role, e.class, e.encoding, e.width, e.fixed_len, e.values, e.extent_count, e.wrapper
            ));
        }
        Ok(())
    })?;
    Ok(out)
}

/// `f(part_no, rows, part)` over `rel`'s committed parts of the visible generation, in manifest order (the row scan's
/// resolution: the catalog's generation, its parts held open).
fn each_part(rel: &Relation<'_>, mut f: impl FnMut(u64, u64, &pgrc_read::OpenPart) -> PgResult<()>) -> PgResult<()> {
    let resolved = crate::scan::resolve_scan_parts_pinned(rel, None)?;
    for (spec, pin) in resolved.scan.parts.iter().zip(&resolved.pins) {
        f(spec.expect.part_no.unwrap_or(0) as u64, spec.rows, pin.part().as_ref())?;
    }
    Ok(())
}

/// The layout lines of `rel` (empty when it has no committed publish).
pub fn part_layout(rel: &Relation<'_>) -> PgResult<String> {
    let jsonb = crate::schema::jsonb_attnos(rel);
    let mut out = String::new();
    each_part(rel, |part_no, rows, part| {
        let key = match part.find_section(SectionKind::SortKey, 0, 0) {
            Some(idx) => {
                let body = crate::footer::meta_section_body(part, idx)?;
                let k = SortKeyRecord::decode(&body).map_err(|e| err(format!("pgrc_part_layout: SortKey record: {e:?}")))?; // alloc: Cold
                let keys: Vec<String> = k
                    .keys
                    .iter()
                    .enumerate()
                    .map(|(i, e)| {
                        let mut s = e.attno.to_string();
                        if e.pad == SORT_KEY_KIND_TEXT_PATH {
                            let p = k.path(i);
                            for (j, step) in p.iter().enumerate() {
                                let arrow = if j + 1 == p.len() { "->>" } else { "->" };
                                s.push_str(&format!("{arrow}'{}'", String::from_utf8_lossy(step)));
                            }
                        }
                        s
                    })
                    .collect();
                if keys.is_empty() { "-".to_string() } else { keys.join("|") }
            }
            None => "-".to_string(),
        };
        let mut lanes: Vec<String> = Vec::new();
        if let Some(paths) = pgrc_read::read_path_table(part).map_err(crate::read_error)? {
            let dirx = part.stream_directory().map_err(crate::read_error)?;
            for (pos, p) in paths.iter().enumerate() {
                let ord = pos as u32 + 1;
                for s in &jsonb {
                    if let Some(v) = dirx.lookup(*s, ord, StreamRole::Values) {
                        let kind = match ShredLaneKind::of_entry(v.entry.class, v.entry.fixed_len) {
                            Some(ShredLaneKind::Text) => "text",
                            Some(ShredLaneKind::Uuid16) => "uuid",
                            Some(ShredLaneKind::NumericFs) => "numeric",
                            Some(ShredLaneKind::Bool) => "bool",
                            None => "?",
                        };
                        lanes.push(format!("{s}:{p}:{kind}"));
                    }
                }
            }
        }
        out.push_str(&format!("part {part_no} rows={rows} key={key} paths={}\n", lanes.join(",")));
        Ok(())
    })?;
    Ok(out)
}
