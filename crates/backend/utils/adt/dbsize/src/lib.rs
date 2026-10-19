//! dbsize.c: pg_size_bytes, pg_size_pretty (int8/numeric), pg_relation_size,
//! pg_database_size (name/oid), pg_tablespace_size (name/oid), pg_table_size,
//! pg_indexes_size, pg_total_relation_size, pg_relation_filenode,
//! pg_filenode_relation, pg_relation_filepath.

use types_error::{PgError, PgResult, ERRCODE_INVALID_PARAMETER_VALUE};

pub mod builtins;
#[cfg(test)]
mod tests;

// size_pretty_units name/unitbits pairs + the "B" alias for bytes.
const UNITS: &[(&str, u32)] =
    &[("bytes", 0), ("kB", 10), ("MB", 20), ("GB", 30), ("TB", 40), ("PB", 50)];

fn c_isspace(c: u8) -> bool {
    c == b' ' || (0x09..=0x0d).contains(&c)
}

// dbsize.c:807-859: the text bytes verbatim in the message and DETAIL (a
// SQL_ASCII database or a byteain alias can hand over non-UTF-8 bytes).
fn invalid_size_msg(s: &[u8]) -> PgError {
    match core::str::from_utf8(s) {
        Ok(s) => PgError::error(format!("invalid size: \"{s}\"")),
        Err(_) => {
            let mut msg = b"invalid size: \"".to_vec();
            msg.extend_from_slice(s);
            msg.push(b'"');
            PgError::error_raw_message(msg)
        }
    }
}

#[track_caller]
#[cold]
fn invalid_size(s: &[u8]) -> Box<PgError> {
    Box::new(invalid_size_msg(s).with_sqlstate(ERRCODE_INVALID_PARAMETER_VALUE))
}

#[track_caller]
#[cold]
fn invalid_unit(arg: &[u8], unit: &[u8]) -> Box<PgError> {
    let e = invalid_size_msg(arg).with_sqlstate(ERRCODE_INVALID_PARAMETER_VALUE);
    let e = match core::str::from_utf8(unit) {
        Ok(unit) => e.with_detail(format!("Invalid size unit: \"{unit}\".")),
        Err(_) => {
            let mut detail = b"Invalid size unit: \"".to_vec();
            detail.extend_from_slice(unit);
            detail.extend_from_slice(b"\".");
            e.with_detail_raw(detail)
        }
    };
    Box::new(
        e.with_hint(
                "Valid units are \"bytes\", \"B\", \"kB\", \"MB\", \"GB\", \"TB\", and \"PB\".",
            ),
    )
}

pub fn pg_size_bytes(arg: impl AsRef<[u8]>) -> PgResult<i64> {
    let arg = arg.as_ref();
    let s = arg;
    let mut p = 0;
    while p < s.len() && c_isspace(s[p]) {
        p += 1;
    }
    let numstart = p;
    let mut e = p;
    let mut have_digits = false;
    if e < s.len() && (s[e] == b'-' || s[e] == b'+') {
        e += 1;
    }
    while e < s.len() && s[e].is_ascii_digit() {
        have_digits = true;
        e += 1;
    }
    if e < s.len() && s[e] == b'.' {
        e += 1;
        while e < s.len() && s[e].is_ascii_digit() {
            have_digits = true;
            e += 1;
        }
    }
    if !have_digits {
        return Err(invalid_size(arg));
    }
    if e < s.len() && (s[e] == b'e' || s[e] == b'E') {
        // strtol tail: optional sign + digits; no digits means the 'E' text
        // is unit input.
        let mut cp = e + 1;
        while cp < s.len() && c_isspace(s[cp]) {
            cp += 1;
        }
        if cp < s.len() && (s[cp] == b'-' || s[cp] == b'+') {
            cp += 1;
        }
        let digs0 = cp;
        while cp < s.len() && s[cp].is_ascii_digit() {
            cp += 1;
        }
        if cp > digs0 {
            e = cp;
        }
    }

    // The scanned number run is ASCII (sign, digits, '.', 'e').
    let numstr = core::str::from_utf8(&arg[numstart..e]).expect("ASCII number run");
    let mut num = adt_numeric::numeric_in(numstr, -1, None)?
        .expect("hard-error path returns Err");

    let mut p = e;
    while p < s.len() && c_isspace(s[p]) {
        p += 1;
    }
    if p < s.len() {
        let mut end = s.len();
        while end > p && c_isspace(s[end - 1]) {
            end -= 1;
        }
        let unit = &arg[p..end];
        let unitbits = UNITS
            .iter()
            .find(|(n, _)| n.as_bytes().eq_ignore_ascii_case(unit))
            .map(|&(_, b)| b)
            .or_else(|| unit.eq_ignore_ascii_case(b"B").then_some(0));
        let Some(unitbits) = unitbits else {
            return Err(invalid_unit(arg, unit));
        };
        let multiplier = 1i64 << unitbits;
        if multiplier > 1 {
            let mul_num = adt_numeric::int64_to_numeric(multiplier);
            num = adt_numeric::numeric_mul_common(mul_num.num(), num.num())?;
        }
    }

    adt_numeric::numeric_int8(num.num())
}
