//! `contrib/dict_xsyn` — extended synonym dictionary: each rules line is a
//! word followed by its synonyms; matching and output of the first word and
//! of the synonyms are separately switchable.

use ::mcx::{alloc_in, vec_with_capacity_in, Mcx, PgVec};
use ::ts_locale::dict_api::{def_get_boolean, def_get_string, DictInitData, LexizeResult};
use ::ts_locale::{could_not_open_error, get_tsearch_config_filename, lowerstr, tsearch_readline_begin, TsLexeme};
use ::types_error::{PgError, PgResult, ERRCODE_INVALID_PARAMETER_VALUE};
use ::types_fmgr::{FmgrInfo, FunctionCallInfoBaseData as Fcinfo, PGFunction};
use datum::Datum;

const LIBRARY: &str = "dict_xsyn";

#[derive(Clone, Copy)]
struct Syn {
    key: (usize, usize),
    line: usize,
}

pub struct DictSyn {
    lines: PgVec<'static, PgVec<'static, u8>>,
    syn: PgVec<'static, Syn>,
    matchorig: bool,
    keeporig: bool,
    matchsynonyms: bool,
    keepsynonyms: bool,
}

impl DictSyn {
    fn key(&self, s: &Syn) -> &[u8] {
        &self.lines[s.line][s.key.0..s.key.1]
    }
}

#[cold]
fn invalid_param(msg: String) -> Box<PgError> {
    PgError::error(msg).with_sqlstate(ERRCODE_INVALID_PARAMETER_VALUE).into()
}

/// find_word: the next whitespace-delimited word at or after `pos`; a word
/// starting with `#` ends the line.
pub fn find_word(line: &[u8], pos: usize) -> Option<(usize, usize)> {
    let mut i = pos;
    while i < line.len() && line[i] != 0 && pg_string::isspace_c_locale(line[i]) {
        i += ::mbutils::pg_mblen(&line[i..]).max(1) as usize;
    }
    if i >= line.len() || line[i] == 0 || line[i] == b'#' {
        return None;
    }
    let start = i;
    while i < line.len() && line[i] != 0 && !pg_string::isspace_c_locale(line[i]) {
        i += ::mbutils::pg_mblen(&line[i..]).max(1) as usize;
    }
    Some((start, i.min(line.len())))
}

/// The (start, end) of every word the rules line contributes as a key.
pub fn line_keys(line: &[u8], matchorig: bool, matchsynonyms: bool) -> Vec<(usize, usize)> {
    let mut keys = Vec::new();
    let mut pos = 0;
    while let Some((start, end)) = find_word(line, pos) {
        if pos != 0 || matchorig {
            keys.push((start, end));
        }
        pos = end;
        if !matchsynonyms {
            break;
        }
    }
    keys
}

/// The words lexize outputs for a matched rules line.
pub fn line_output(line: &[u8], keeporig: bool, keepsynonyms: bool) -> Vec<(usize, usize)> {
    let mut out = Vec::new();
    let mut pos = 0;
    while let Some((start, end)) = find_word(line, pos) {
        if pos != 0 || keeporig {
            out.push((start, end));
        }
        pos = end;
        if !keepsynonyms {
            break;
        }
    }
    out
}

fn compare(a: &[u8], b: &[u8]) -> i32 {
    match a.cmp(b) {
        core::cmp::Ordering::Less => -1,
        core::cmp::Ordering::Equal => 0,
        core::cmp::Ordering::Greater => 1,
    }
}

fn read_dictionary(d: &mut DictSyn, mcx: Mcx<'static>, filename: &[u8]) -> PgResult<()> {
    let path = get_tsearch_config_filename(mcx, filename, "rules")?;
    let mut rd = match tsearch_readline_begin(mcx, &path) {
        Ok(rd) => rd,
        Err(errno) => return Err(could_not_open_error("synonym", &path, errno).into()),
    };
    let mut syn: PgVec<'static, Syn> = PgVec::new_in(mcx);
    loop {
        let next = rd.readline();
        let Some(line) = rd.with_context(next)? else {
            break;
        };
        if line.first().is_none_or(|&b| b == 0) {
            continue;
        }
        let value = rd.with_context(lowerstr(mcx, &line))?;
        let keys = line_keys(&value, d.matchorig, d.matchsynonyms);
        if keys.is_empty() {
            continue;
        }
        let lineno = d.lines.len();
        d.lines.push(value);
        for key in keys {
            syn.push(Syn { key, line: lineno });
        }
    }

    // pg_qsort's equal-key output order decides which duplicate bsearch
    // lands on; sorting an index permutation reproduces C's order.
    let mut order: PgVec<'static, usize> = vec_with_capacity_in(mcx, syn.len())?;
    order.extend(0..syn.len());
    if syn.len() > 1 {
        let lines = &d.lines;
        let keyof = |s: &Syn| &lines[s.line][s.key.0..s.key.1];
        ::pg_qsort::pg_qsort(&mut order, |&a, &b| compare(keyof(&syn[a]), keyof(&syn[b])));
    }
    let mut sorted: PgVec<'static, Syn> = vec_with_capacity_in(mcx, syn.len())?;
    for &i in order.iter() {
        sorted.push(syn[i]);
    }
    d.syn = sorted;
    Ok(())
}

pub fn dxsyn_init(init: &DictInitData<'static>) -> PgResult<DictSyn> {
    let mut d = DictSyn {
        lines: PgVec::new_in(init.mcx),
        syn: PgVec::new_in(init.mcx),
        matchorig: true,
        keeporig: true,
        matchsynonyms: false,
        keepsynonyms: true,
    };
    let mut filename: Option<&[u8]> = None;
    for (i, (name, value)) in init.dict_options.iter().enumerate() {
        let boolean =
            || def_get_boolean(name, value.as_ref().map(|v| v.as_slice()), init.int_options[i]);
        match name.as_slice() {
            b"matchorig" => d.matchorig = boolean()?,
            b"keeporig" => d.keeporig = boolean()?,
            b"matchsynonyms" => d.matchsynonyms = boolean()?,
            b"keepsynonyms" => d.keepsynonyms = boolean()?,
            b"rules" => filename = Some(def_get_string(name, value)?),
            _ => {
                return Err(invalid_param(format!(
                    "unrecognized xsyn parameter: \"{}\"",
                    String::from_utf8_lossy(name)
                )))
            }
        }
    }
    if let Some(filename) = filename {
        read_dictionary(&mut d, init.mcx, filename)?;
    }
    Ok(d)
}

// bsearch(3) probe order: (l + u) / 2, first equal element met wins.
fn c_bsearch(d: &DictSyn, key: &[u8]) -> Option<usize> {
    let (mut l, mut u) = (0usize, d.syn.len());
    while l < u {
        let idx = (l + u) / 2;
        match key.cmp(d.key(&d.syn[idx])) {
            core::cmp::Ordering::Less => u = idx,
            core::cmp::Ordering::Greater => l = idx + 1,
            core::cmp::Ordering::Equal => return Some(idx),
        }
    }
    None
}

pub fn dxsyn_lexize<'mcx>(
    mcx: Mcx<'mcx>,
    d: &DictSyn,
    token: &[u8],
) -> PgResult<Option<LexizeResult<'mcx>>> {
    if token.is_empty() || d.syn.is_empty() {
        return Ok(None);
    }
    let token = &token[..token.iter().position(|&b| b == 0).unwrap_or(token.len())];
    let key = lowerstr(mcx, token)?;
    let Some(idx) = c_bsearch(d, &key) else {
        return Ok(None);
    };
    let value = &d.lines[d.syn[idx].line];
    let mut out = PgVec::new_in(mcx);
    for (start, end) in line_output(value, d.keeporig, d.keepsynonyms) {
        let mut lexeme = vec_with_capacity_in(mcx, end - start)?;
        lexeme.extend_from_slice(&value[start..end]);
        out.push(TsLexeme { nvariant: 0, flags: 0, lexeme });
    }
    Ok(Some(LexizeResult(out)))
}

fn fc_dxsyn_init(_flinfo: Option<&mut FmgrInfo>, fcinfo: &mut Fcinfo) -> PgResult<Datum> {
    // SAFETY: arg0 is the DictInitData built by the ts_cache dictionary loader.
    let init = unsafe { &*(fcinfo.arg(0).as_usize() as *const DictInitData<'static>) };
    let d = dxsyn_init(init)?;
    let (ptr, _) = ::mcx::PgBox::into_raw_with_allocator(alloc_in(init.mcx, d)?);
    Ok(Datum::from_usize(ptr as usize))
}

fn fc_dxsyn_lexize(_flinfo: Option<&mut FmgrInfo>, fcinfo: &mut Fcinfo) -> PgResult<Datum> {
    // SAFETY: arg0 came from fc_dxsyn_init and outlives the cache entry.
    let d = unsafe { &*(fcinfo.arg(0).as_usize() as *const DictSyn) };
    let len = fcinfo.arg(2).as_i32().max(0) as usize;
    // SAFETY: dict_api lexize convention — arg1 points at `len` live bytes.
    let token = unsafe { core::slice::from_raw_parts(fcinfo.arg(1).as_usize() as *const u8, len) };
    // SAFETY: the arming context outlives this call.
    let mcx = unsafe { fcinfo.result_mcx_detached() };
    match dxsyn_lexize(mcx, d, token)? {
        None => Ok(Datum::from_usize(0)),
        Some(r) => {
            let (ptr, _) = ::mcx::PgBox::into_raw_with_allocator(alloc_in(mcx, r)?);
            Ok(Datum::from_usize(ptr as usize))
        }
    }
}

fn lookup(function: &str) -> Option<PGFunction> {
    Some(match function {
        "dxsyn_init" => fc_dxsyn_init,
        "dxsyn_lexize" => fc_dxsyn_lexize,
        _ => return None,
    })
}

pub fn init_seams() {
    dfmgr::register_builtin_library(dfmgr::BuiltinLibraryEntry {
        name: LIBRARY,
        lookup,
        pg_init: None,
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    const LINE: &[u8] = b"supernova sn sne 1987a\n";

    fn words(line: &[u8], spans: Vec<(usize, usize)>) -> Vec<&[u8]> {
        spans.into_iter().map(|(s, e)| &line[s..e]).collect()
    }

    #[test]
    fn comments_and_blank_lines_have_no_words() {
        assert_eq!(find_word(b"# comment\n", 0), None);
        assert_eq!(find_word(b"   \n", 0), None);
        assert_eq!(words(b"a b #c d", line_output(b"a b #c d", true, true)), vec![&b"a"[..], b"b"]);
    }

    #[test]
    fn key_selection_follows_match_flags() {
        assert_eq!(words(LINE, line_keys(LINE, true, false)), vec![&b"supernova"[..]]);
        assert_eq!(
            words(LINE, line_keys(LINE, true, true)),
            vec![&b"supernova"[..], b"sn", b"sne", b"1987a"]
        );
        assert_eq!(words(LINE, line_keys(LINE, false, true)), vec![&b"sn"[..], b"sne", b"1987a"]);
        assert!(line_keys(LINE, false, false).is_empty());
    }

    #[test]
    fn output_follows_keep_flags() {
        assert_eq!(
            words(LINE, line_output(LINE, true, true)),
            vec![&b"supernova"[..], b"sn", b"sne", b"1987a"]
        );
        assert_eq!(words(LINE, line_output(LINE, false, true)), vec![&b"sn"[..], b"sne", b"1987a"]);
        assert_eq!(words(LINE, line_output(LINE, true, false)), vec![&b"supernova"[..]]);
        assert!(line_output(LINE, false, false).is_empty());
    }

    #[test]
    fn library_exports_template_methods() {
        assert!(lookup("dxsyn_init").is_some());
        assert!(lookup("dxsyn_lexize").is_some());
        assert!(lookup("dintdict_init").is_none());
    }
}
