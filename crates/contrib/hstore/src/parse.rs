//! parse_hstore / get_val (hstore_io.c): the `key => value, ...` FSM with
//! `"`-quoting and `\`-escapes; an unquoted case-insensitive `null` value is
//! SQL NULL.

use types_error::{PgError, PgResult, ERRCODE_SYNTAX_ERROR};

use crate::repr::Pair;
use crate::{check_key_len, check_val_len};

// scanner_isspace (scansup.c): must match scan.l's {space} class, which
// includes \v (0x0b) since PG 16. Missing \v was a real divergence found by
// hstore_diff (lane p1-mb-contribc).
fn is_space(c: u8) -> bool {
    matches!(c, b' ' | b'\t' | b'\n' | b'\r' | 0x0b | 0x0c)
}

enum Gv {
    WaitVal,
    InVal,
    InEscVal,
    WaitEscIn,
    WaitEscEscIn,
}

// RESIZEPRSBUF (hstore_io.c:51): the token buffer doubles from 32 bytes
// before each append that would fill it; repalloc's MaxAllocSize guard fires
// at the 2^30 step, so a single 512 MiB token is an error.
fn resize_prsbuf(len: usize, wordlen: usize) -> PgResult<usize> {
    if len + 1 >= wordlen {
        let grown = wordlen * 2;
        mcx::check_alloc_size(grown)?;
        return Ok(grown);
    }
    Ok(wordlen)
}

// parse_hstore's Pairs array (hstore_io.c:242) starts at 16 entries and
// doubles when full; the repalloc to 2^25 * sizeof(Pairs) exceeds
// MaxAllocSize, so 16777217 pairs of input are an error however many
// distinct keys they hold.
const SIZEOF_PAIRS: usize = 40;

fn grow_pairs(pcur: usize, plen: usize) -> PgResult<usize> {
    if pcur >= plen {
        let grown = plen * 2;
        mcx::check_alloc_size(grown * SIZEOF_PAIRS)?;
        return Ok(grown);
    }
    Ok(plen)
}

// get_val: one token starting at ptr; Ok(None) is EOF at token start.
fn get_val(input: &[u8], mut ptr: usize, ignoreeq: bool) -> PgResult<Option<(Vec<u8>, bool, usize)>> {
    let mut st = Gv::WaitVal;
    let mut wordlen = 32usize;
    let mut word: Vec<u8> = Vec::with_capacity(wordlen);
    let mut escaped = false;
    loop {
        let c = at(input, ptr);
        match st {
            Gv::WaitVal => {
                if c == b'"' {
                    escaped = true;
                    st = Gv::InEscVal;
                } else if c == 0 {
                    return Ok(None);
                } else if c == b'=' && !ignoreeq {
                    return Err(syntax_error(input, ptr));
                } else if c == b'\\' {
                    st = Gv::WaitEscIn;
                } else if !is_space(c) {
                    wordlen = resize_prsbuf(word.len(), wordlen)?;
                    word.push(c);
                    st = Gv::InVal;
                }
            }
            Gv::InVal => {
                if c == b'\\' {
                    st = Gv::WaitEscIn;
                } else if c == b'=' && !ignoreeq {
                    return Ok(Some((word, escaped, ptr - 1)));
                } else if c == b',' && ignoreeq {
                    return Ok(Some((word, escaped, ptr - 1)));
                } else if is_space(c) {
                    return Ok(Some((word, escaped, ptr)));
                } else if c == 0 {
                    return Ok(Some((word, escaped, ptr.wrapping_sub(1))));
                } else {
                    wordlen = resize_prsbuf(word.len(), wordlen)?;
                    word.push(c);
                }
            }
            Gv::InEscVal => {
                if c == b'\\' {
                    st = Gv::WaitEscEscIn;
                } else if c == b'"' {
                    return Ok(Some((word, escaped, ptr)));
                } else if c == 0 {
                    return Err(eof_error());
                } else {
                    wordlen = resize_prsbuf(word.len(), wordlen)?;
                    word.push(c);
                }
            }
            Gv::WaitEscIn => {
                if c == 0 {
                    return Err(eof_error());
                }
                wordlen = resize_prsbuf(word.len(), wordlen)?;
                word.push(c);
                st = Gv::InVal;
            }
            Gv::WaitEscEscIn => {
                if c == 0 {
                    return Err(eof_error());
                }
                wordlen = resize_prsbuf(word.len(), wordlen)?;
                word.push(c);
                st = Gv::InEscVal;
            }
        }
        ptr = ptr.wrapping_add(1);
    }
}

enum St {
    Key,
    Eq,
    Gt,
    Val,
    Del,
}

pub fn parse_hstore(input: &[u8]) -> PgResult<Vec<Pair>> {
    let mut st = St::Key;
    let mut ptr = 0usize;
    let mut plen = 16usize;
    let mut pairs: Vec<Pair> = Vec::with_capacity(plen);
    let mut cur_key: Option<Vec<u8>> = None;
    loop {
        match st {
            St::Key => match get_val(input, ptr, false)? {
                None => return Ok(pairs),
                Some((word, _esc, newptr)) => {
                    ptr = newptr;
                    plen = grow_pairs(pairs.len(), plen)?;
                    check_key_len(word.len())?;
                    cur_key = Some(word);
                    st = St::Eq;
                }
            },
            St::Eq => {
                let c = at(input, ptr);
                if c == b'=' {
                    st = St::Gt;
                } else if c == 0 {
                    return Err(eof_error());
                } else if !is_space(c) {
                    return Err(syntax_error(input, ptr));
                }
            }
            St::Gt => {
                let c = at(input, ptr);
                if c == b'>' {
                    st = St::Val;
                } else if c == 0 {
                    return Err(eof_error());
                } else {
                    return Err(syntax_error(input, ptr));
                }
            }
            St::Val => match get_val(input, ptr, true)? {
                None => return Err(eof_error()),
                Some((word, escaped, newptr)) => {
                    ptr = newptr;
                    check_val_len(word.len())?;
                    let isnull =
                        word.len() == 4 && !escaped && word.eq_ignore_ascii_case(b"null");
                    crate::check_pair_count(pairs.len() + 1)?;
                    pairs.try_reserve(1).map_err(|_| ::mcx::oom_named("hstore_in", 56))?;
                    pairs.push(Pair {
                        key: cur_key.take().expect("key set before value"),
                        val: (!isnull).then_some(word),
                        needfree: true,
                    });
                    st = St::Del;
                }
            },
            St::Del => {
                let c = at(input, ptr);
                if c == b',' {
                    st = St::Key;
                } else if c == 0 {
                    return Ok(pairs);
                } else if !is_space(c) {
                    return Err(syntax_error(input, ptr));
                }
            }
        }
        ptr = ptr.wrapping_add(1);
    }
}

#[inline]
fn at(input: &[u8], ptr: usize) -> u8 {
    input.get(ptr).copied().unwrap_or(0)
}

// prssyntaxerror: the snippet is the pg_mblen_cstr bytes at ptr in the
// server encoding, sent as C sends them.
#[track_caller]
#[cold]
fn syntax_error(input: &[u8], ptr: usize) -> Box<PgError> {
    let snippet: &[u8] = match input.get(ptr) {
        None | Some(0) => &[],
        Some(_) => {
            let rest = &input[ptr..];
            match mbutils::pg_mblen_with_len(rest, rest.len() as i32) {
                Ok(n) => &rest[..n as usize],
                Err(e) => return e,
            }
        }
    };
    let mut msg = b"syntax error in hstore, near \"".to_vec();
    msg.extend_from_slice(snippet);
    msg.extend_from_slice(format!("\" at position {ptr}").as_bytes());
    Box::new(PgError::error_raw_message(msg).with_sqlstate(ERRCODE_SYNTAX_ERROR))
}

#[track_caller]
#[cold]
fn eof_error() -> Box<PgError> {
    Box::new(
        PgError::error("syntax error in hstore: unexpected end of string")
            .with_sqlstate(ERRCODE_SYNTAX_ERROR),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    // RESIZEPRSBUF doubles 32 -> 64 -> ... and repalloc rejects the 2^30
    // request, so byte 536870912 of one token is C's XX000; the pair-array
    // guard (hstore_to_array_internal) trips at 67108864 pairs.
    #[test]
    fn scratch_buffer_growth_stops_at_max_alloc_size() {
        assert_eq!(resize_prsbuf(5, 32).unwrap(), 32);
        assert_eq!(resize_prsbuf(31, 32).unwrap(), 64);
        assert_eq!(resize_prsbuf(536870910, 536870912).unwrap(), 536870912);
        let err = resize_prsbuf(536870911, 536870912).unwrap_err();
        assert_eq!(err.message(), "invalid memory alloc request size 1073741824");
        assert!(crate::to_array_alloc_guard(67108863).is_ok());
        let err = crate::to_array_alloc_guard(67108864).unwrap_err();
        assert_eq!(err.message(), "invalid memory alloc request size 1073741824");
    }

    fn kv(input: &str) -> Vec<(String, Option<String>)> {
        parse_hstore(input.as_bytes())
            .unwrap()
            .into_iter()
            .map(|p| {
                (
                    String::from_utf8(p.key).unwrap(),
                    p.val.map(|v| String::from_utf8(v).unwrap()),
                )
            })
            .collect()
    }

    // hstore_io.c:242: the 16777217th pair asks for 2^25 Pairs (1342177280
    // bytes); 16777216 pairs fit in the 2^24 array.
    #[test]
    fn pairs_array_doubling_stops_at_max_alloc_size() {
        assert_eq!(grow_pairs(0, 16).unwrap(), 16);
        assert_eq!(grow_pairs(16, 16).unwrap(), 32);
        assert_eq!(grow_pairs(16777215, 16777216).unwrap(), 16777216);
        let err = grow_pairs(16777216, 16777216).unwrap_err();
        assert_eq!(err.message(), "invalid memory alloc request size 1342177280");
    }

    #[test]
    fn syntax_error_snippet_carries_raw_server_bytes() {
        let err = parse_hstore(b"a=>b \xe9").unwrap_err();
        assert_eq!(
            err.message_raw.as_deref(),
            Some(b"syntax error in hstore, near \"\xe9\" at position 5".as_slice())
        );
        let err = parse_hstore(b"a=b").unwrap_err();
        assert_eq!(
            err.message_raw.as_deref(),
            Some(b"syntax error in hstore, near \"b\" at position 2".as_slice())
        );
    }

    #[test]
    fn basic_forms() {
        assert_eq!(kv("a=>b"), vec![("a".into(), Some("b".into()))]);
        assert_eq!(kv(" a => NULL "), vec![("a".into(), None)]);
        assert_eq!(kv(r#""x y"=>"z,w""#), vec![("x y".into(), Some("z,w".into()))]);
        assert_eq!(kv(r#"a=>"NULL""#), vec![("a".into(), Some("NULL".into()))]);
        assert_eq!(kv(r"a\=>b=>1"), vec![("a=>b".into(), Some("1".into()))]);
        assert!(kv("").is_empty());
    }

    #[test]
    fn syntax_errors() {
        assert!(parse_hstore(b"a").is_err());
        assert!(parse_hstore(b"a=>").is_err());
        assert!(parse_hstore(b"a=b").is_err());
        assert!(parse_hstore(b"a=>b, ").is_ok());
    }
}
