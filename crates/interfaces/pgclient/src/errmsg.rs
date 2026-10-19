//! libpq's pqBuildErrorMessage3 (PQERRORS_DEFAULT verbosity,
//! PQSHOW_CONTEXT_ERRORS) + reportErrorPosition (fe-protocol3.c:1031/1202):
//! the text PQerrorMessage / the default notice processor produce for an
//! ErrorResponse / NoticeResponse, LINE/caret display included. Same port as
//! psql's errmsg.rs; the widths assume UTF-8.

use crate::ErrorFields;

const DISPLAY_SIZE: i32 = 60;
const MIN_RIGHT_CUT: i32 = 10;

/// "SEVERITY:  primary\n[LINE n: ...\n     ^\n][DETAIL:  ...\n][HINT:  ...\n]
/// [QUERY:  ...\n][CONTEXT:  ...\n]" — CONTEXT only for errors (libpq's
/// default show_context). `query` = the command in flight (cmd_queue_head's
/// query text), consulted only when the server reports a position.
pub fn build_message(f: &ErrorFields, query: Option<&str>, is_error: bool) -> String {
    let mut msg = String::new();
    if !f.severity.is_empty() {
        msg.push_str(&format!("{}:  ", f.severity));
    }
    msg.push_str(&f.primary);

    let mut querytext: Option<&str> = None;
    let mut querypos: i32 = 0;
    if !f.position.is_empty() {
        if let Some(q) = query {
            querytext = Some(q);
            querypos = atoi(&f.position);
        } else {
            msg.push_str(&format!(" at character {}", f.position));
        }
    } else if !f.internal_position.is_empty() {
        if !f.internal_query.is_empty() {
            querytext = Some(&f.internal_query);
            querypos = atoi(&f.internal_position);
        } else {
            msg.push_str(&format!(" at character {}", f.internal_position));
        }
    }
    msg.push('\n');

    if let (Some(q), true) = (querytext, querypos > 0) {
        report_error_position(&mut msg, q, querypos);
    }
    if !f.detail.is_empty() {
        msg.push_str(&format!("DETAIL:  {}\n", f.detail));
    }
    if !f.hint.is_empty() {
        msg.push_str(&format!("HINT:  {}\n", f.hint));
    }
    if !f.internal_query.is_empty() {
        msg.push_str(&format!("QUERY:  {}\n", f.internal_query));
    }
    if is_error && !f.context.is_empty() {
        msg.push_str(&format!("CONTEXT:  {}\n", f.context));
    }
    msg
}

// atoi(3): leading whitespace, optional sign, digits; 0 when nothing parses.
fn atoi(s: &str) -> i32 {
    let t = s.trim_start_matches(|c: char| c.is_ascii() && pg_string::isspace_c_locale(c as u8));
    let (neg, digits) = match t.strip_prefix('-') {
        Some(r) => (true, r),
        None => (false, t.strip_prefix('+').unwrap_or(t)),
    };
    let end = digits.bytes().position(|b| !b.is_ascii_digit()).unwrap_or(digits.len());
    let v: i64 = digits[..end].parse().unwrap_or(0);
    let v = v.clamp(i32::MIN as i64, i32::MAX as i64) as i32;
    if neg {
        v.wrapping_neg()
    } else {
        v
    }
}

fn report_error_position(msg: &mut String, query: &str, loc: i32) {
    let mut loc = loc - 1;
    if loc < 0 {
        return;
    }
    let wquery: String = query.replace('\t', " ");
    let chars: Vec<char> = wquery.chars().collect();

    let mut scridx: Vec<i32> = Vec::with_capacity(chars.len() + 1);
    let mut scroffset: i32 = 0;
    let mut loc_line: i32 = 1;
    let mut ibeg: usize = 0;
    let mut iend: i32 = -1;
    let mut cno: usize = 0;
    while cno < chars.len() {
        let ch = chars[cno];
        scridx.push(scroffset);
        if ch == '\r' || ch == '\n' {
            if (cno as i32) < loc {
                if ch == '\r' || cno == 0 || chars[cno - 1] != '\r' {
                    loc_line += 1;
                }
                ibeg = cno + 1;
            } else {
                iend = cno as i32;
                break;
            }
        }
        let mut buf = [0u8; 4];
        let s = ch.encode_utf8(&mut buf);
        let mut w = char_dsplen(s);
        if w <= 0 {
            w = 1;
        }
        scroffset += w;
        cno += 1;
    }
    if iend < 0 {
        iend = cno as i32;
        scridx.push(scroffset);
    }
    let iend = iend as usize;

    if loc as usize > cno {
        return;
    }
    if loc as usize >= scridx.len() {
        loc = (scridx.len() - 1) as i32;
    }
    let locu = loc as usize;

    let mut beg_trunc = false;
    let mut end_trunc = false;
    let mut ibeg = ibeg;
    let mut iend = iend;
    if scridx[iend] - scridx[ibeg] > DISPLAY_SIZE {
        if scridx[ibeg] + DISPLAY_SIZE >= scridx[locu] + MIN_RIGHT_CUT {
            while scridx[iend] - scridx[ibeg] > DISPLAY_SIZE {
                iend -= 1;
            }
            end_trunc = true;
        } else {
            while scridx[locu] + MIN_RIGHT_CUT < scridx[iend] {
                iend -= 1;
                end_trunc = true;
            }
            while scridx[iend] - scridx[ibeg] > DISPLAY_SIZE {
                ibeg += 1;
                beg_trunc = true;
            }
        }
    }

    let mut prefix = format!("LINE {loc_line}: ");
    if beg_trunc {
        prefix.push_str("...");
    }
    let prefix_width: i32 = prefix
        .chars()
        .map(|c| {
            let mut b = [0u8; 4];
            let w = char_dsplen(c.encode_utf8(&mut b));
            if w <= 0 {
                1
            } else {
                w
            }
        })
        .sum();
    msg.push_str(&prefix);
    let excerpt: String = chars[ibeg..iend].iter().collect();
    msg.push_str(&excerpt);
    if end_trunc {
        msg.push_str("...");
    }
    msg.push('\n');
    let caret_col = prefix_width + (scridx[locu] - scridx[ibeg]);
    for _ in 0..caret_col {
        msg.push(' ');
    }
    msg.push('^');
    msg.push('\n');
}

fn char_dsplen(s: &str) -> i32 {
    wchar::pg_encoding_dsplen(wchar::PG_UTF8, s.as_bytes())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn caret_basic() {
        let f = ErrorFields {
            severity: "ERROR".into(),
            primary: "relation \"nosuch\" does not exist".into(),
            position: "15".into(),
            ..Default::default()
        };
        let m = build_message(&f, Some("select * from nosuch;"), true);
        assert_eq!(
            m,
            "ERROR:  relation \"nosuch\" does not exist\nLINE 1: select * from nosuch;\n                      ^\n"
        );
    }

    #[test]
    fn caret_second_line_and_no_query() {
        let f = ErrorFields {
            severity: "ERROR".into(),
            primary: "x".into(),
            position: "10".into(),
            ..Default::default()
        };
        let m = build_message(&f, Some("select\n1 bogus;"), true);
        assert!(m.ends_with("LINE 2: 1 bogus;\n          ^\n"), "{m}");
        assert_eq!(build_message(&f, None, true), "ERROR:  x at character 10\n");
    }

    #[test]
    fn notice_hides_context_and_internal_query_is_reported() {
        let f = ErrorFields {
            severity: "NOTICE".into(),
            primary: "n".into(),
            context: "PL/pgSQL function f() line 1".into(),
            detail: "d".into(),
            hint: "h".into(),
            ..Default::default()
        };
        assert_eq!(build_message(&f, Some("select f()"), false), "NOTICE:  n\nDETAIL:  d\nHINT:  h\n");
        assert_eq!(
            build_message(&f, Some("select f()"), true),
            "NOTICE:  n\nDETAIL:  d\nHINT:  h\nCONTEXT:  PL/pgSQL function f() line 1\n"
        );
        let f = ErrorFields {
            severity: "ERROR".into(),
            primary: "column \"zz\" does not exist".into(),
            internal_position: "8".into(),
            internal_query: "SELECT zz".into(),
            ..Default::default()
        };
        assert_eq!(
            build_message(&f, Some("select f()"), true),
            "ERROR:  column \"zz\" does not exist\nLINE 1: SELECT zz\n               ^\nQUERY:  SELECT zz\n"
        );
    }
}
