//! src/common/percentrepl.c

use elog::ereport;
use types_error::{ErrorLocation, PgResult, ERRCODE_INVALID_PARAMETER_VALUE, ERROR};

fn loc() -> ErrorLocation {
    ErrorLocation::new(file!(), line!() as i32, "replace_percent_placeholders")
}

// C varargs (letters, ...) becomes a (letter, value) slice; None values mirror
// C NULL (placeholder present in letters but unsupported at this call site).
pub fn replace_percent_placeholders(
    instr: &str,
    param_name: &str,
    values: &[(char, Option<&str>)],
) -> PgResult<String> {
    let mut result = String::with_capacity(instr.len());
    // percentrepl.c walks BYTES: the placeholder after '%' is one byte, and an
    // unknown one is reported with %c (a multibyte lead byte goes out alone).
    let bytes = instr.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] != b'%' {
            let c = instr[i..].chars().next().expect("char boundary");
            result.push(c);
            i += c.len_utf8();
            continue;
        }
        match bytes.get(i + 1) {
            Some(b'%') => {
                result.push('%');
                i += 2;
            }
            None => {
                ereport(ERROR)
                    .errcode(ERRCODE_INVALID_PARAMETER_VALUE)
                    .errmsg(format!("invalid value for parameter \"{param_name}\": \"{instr}\""))
                    .errdetail("String ends unexpectedly after escape character \"%\".")
                    .finish(loc())?;
                unreachable!()
            }
            Some(&p) => {
                let found = values
                    .iter()
                    .find(|(l, _)| u32::from(*l) == u32::from(p))
                    .and_then(|(_, v)| *v);
                match found {
                    Some(val) => {
                        result.push_str(val);
                        i += 2;
                    }
                    None => {
                        let mut detail = b"String contains unexpected placeholder \"%".to_vec();
                        detail.push(p);
                        detail.extend_from_slice(b"\".");
                        ereport(ERROR)
                            .errcode(ERRCODE_INVALID_PARAMETER_VALUE)
                            .errmsg(format!(
                                "invalid value for parameter \"{param_name}\": \"{instr}\""
                            ))
                            .errdetail_raw(detail)
                            .finish(loc())?;
                        unreachable!()
                    }
                }
            }
        }
    }
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn double_percent_is_literal() {
        assert_eq!(replace_percent_placeholders("100%%", "p", &[]).unwrap(), "100%");
    }

    #[test]
    fn placeholder_substitution() {
        let v = [('f', Some("file")), ('p', Some("path"))];
        assert_eq!(
            replace_percent_placeholders("cp %p %f", "archive_command", &v).unwrap(),
            "cp path file"
        );
    }

    #[test]
    fn trailing_percent_errors() {
        let err = replace_percent_placeholders("abc%", "p", &[]).unwrap_err();
        assert!(err.message().contains("invalid value for parameter"));
    }

    #[test]
    fn unsupported_placeholder_errors() {
        let err = replace_percent_placeholders("%z", "p", &[('f', Some("x"))]).unwrap_err();
        assert!(err.message().contains("invalid value for parameter"));
    }

    // regression: percentrepl.c:125 reports the single byte after '%'.
    #[test]
    fn unknown_multibyte_placeholder_reports_its_first_byte() {
        let err = replace_percent_placeholders("%\u{e9}", "p", &[('f', Some("x"))]).unwrap_err();
        assert_eq!(err.message(), "invalid value for parameter \"p\": \"%\u{e9}\"");
        assert_eq!(
            err.detail_raw.as_deref(),
            Some(b"String contains unexpected placeholder \"%\xC3\".".as_slice())
        );
        let err = replace_percent_placeholders("%z", "p", &[('f', Some("x"))]).unwrap_err();
        assert_eq!(err.detail(), Some("String contains unexpected placeholder \"%z\"."));
        assert_eq!(
            replace_percent_placeholders("caf\u{e9} %f \u{e9}%%", "p", &[('f', Some("x"))]).unwrap(),
            "caf\u{e9} x \u{e9}%"
        );
    }

    #[test]
    fn null_value_is_unsupported() {
        let err = replace_percent_placeholders("%r", "p", &[('r', None)]).unwrap_err();
        assert!(err.message().contains("invalid value for parameter"));
    }
}
