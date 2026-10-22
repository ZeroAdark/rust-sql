//! Vixie cron's crontab-line parser as pg_cron ships it (entry.c

use super::*;

/// MAX_FILE_BUFFER_LENGTH (cron.h): schedules this long or longer fail.
const MAX_FILE_BUFFER_LENGTH: usize = 1000;
/// MAX_TEMPSTR: get_number's alnum token cap.
const MAX_TEMPSTR: usize = 1000;
/// MAX_COMMAND: get_string's cap for the @word.
const MAX_COMMAND: usize = 1000;

const MONTH_NAMES: [&str; 12] =
    ["Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec"];
const DOW_NAMES: [&str; 8] = ["Sun", "Mon", "Tue", "Wed", "Thu", "Fri", "Sat", "Sun"];

/// misc.c get_char's return alphabet: a byte, the one-shot '\0' at end of
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Ch {
    C(u8),
    Eof,
}
use Ch::{Eof, C};

/// cron.h `file_buffer` (data + pointer + unget stack) — the FILE* hack.
struct Buf<'a> {
    data: &'a [u8],
    pointer: usize,
    unget: Vec<Ch>,
}

impl Buf<'_> {
    fn get_char(&mut self) -> Ch {
        if let Some(c) = self.unget.pop() {
            return c;
        }
        if self.pointer == self.data.len() {
            self.pointer += 1;
            C(0)
        } else if self.pointer > self.data.len() {
            Eof
        } else {
            let c = self.data[self.pointer];
            self.pointer += 1;
            C(c)
        }
    }

    fn unget_char(&mut self, ch: Ch) {
        self.unget.push(ch);
    }

    /// get_string(str, MAX_COMMAND, file, " \t\n"): strchr(terms, '\0')
    fn get_string(&mut self, max: usize) -> (Vec<u8>, Ch) {
        let mut out = Vec::new();
        loop {
            let ch = self.get_char();
            match ch {
                Eof => return (out, Eof),
                C(c) if c == b' ' || c == b'\t' || c == b'\n' || c == 0 => return (out, ch),
                C(c) => {
                    if out.len() < max - 1 {
                        out.push(c);
                    }
                }
            }
        }
    }

    fn skip_comments(&mut self) {
        let mut ch;
        loop {
            ch = self.get_char();
            if ch == Eof {
                return;
            }
            while ch == C(b' ') || ch == C(b'\t') {
                ch = self.get_char();
            }
            if ch == Eof {
                return;
            }
            if ch != C(b'\n') && ch != C(b'#') {
                break;
            }
            while ch != C(b'\n') && ch != Eof {
                ch = self.get_char();
            }
        }
        self.unget_char(ch);
    }
}

fn is_blank(ch: Ch) -> bool {
    ch == C(b' ') || ch == C(b'\t')
}

fn is_alnum(ch: Ch) -> bool {
    matches!(ch, C(c) if c.is_ascii_alphanumeric())
}

fn set_element(bits: &mut u64, low: i32, high: i32, number: i32) -> bool {
    if number < low || number > high {
        return false;
    }
    *bits |= 1 << (number - low);
    true
}

fn set_range(bits: &mut u64, low: i32, high: i32, start: i32, stop: i32, step: i32) -> bool {
    if start < low || stop > high || step <= 0 {
        return false;
    }
    let mut i = start - low;
    while i <= stop - low {
        *bits |= 1 << i;
        i += step;
    }
    true
}

/// get_number: collect alnums, resolve a name (case-insensitive) against
fn get_number(buf: &mut Buf, low: i32, names: &[&str], mut ch: Ch) -> Option<(i32, Ch)> {
    let mut temp: Vec<u8> = Vec::new();
    let mut all_digits = true;
    while is_alnum(ch) {
        if temp.len() + 1 >= MAX_TEMPSTR {
            return None;
        }
        let C(c) = ch else { unreachable!() };
        temp.push(c);
        if !c.is_ascii_digit() {
            all_digits = false;
        }
        ch = buf.get_char();
    }
    if temp.is_empty() {
        return None;
    }
    for (i, name) in names.iter().enumerate() {
        if name.as_bytes().eq_ignore_ascii_case(&temp) {
            return Some((i as i32 + low, ch));
        }
    }
    if all_digits {
        let mut v: i64 = 0;
        for &d in &temp {
            v = v.saturating_mul(10).saturating_add((d - b'0') as i64);
        }
        let v = v.min(i64::from(i32::MAX)) as i32;
        return Some((v, ch));
    }
    None
}

/// get_range: number | number "-" number [ "/" number ] | "*" [ "/" number ].
fn get_range(buf: &mut Buf, bits: &mut u64, low: i32, high: i32, names: &[&str], mut ch: Ch) -> Option<Ch> {
    let (num1, num2);
    if ch == C(b'*') {
        num1 = low;
        num2 = high;
        ch = buf.get_char();
        if ch == Eof {
            return None;
        }
    } else {
        let (n1, c) = get_number(buf, low, names, ch)?;
        num1 = n1;
        ch = c;
        if ch != C(b'-') {
            if ch == C(b'/') {
                return None;
            }
            if !set_element(bits, low, high, num1) {
                return None;
            }
            return Some(ch);
        }
        ch = buf.get_char();
        if ch == Eof {
            return None;
        }
        let (n2, c) = get_number(buf, low, names, ch)?;
        num2 = n2;
        ch = c;
    }
    let num3;
    if ch == C(b'/') {
        ch = buf.get_char();
        if ch == Eof {
            return None;
        }
        let (n3, c) = get_number(buf, 0, &[], ch)?;
        if n3 <= 0 {
            return None;
        }
        num3 = n3;
        ch = c;
    } else {
        num3 = 1;
    }
    if num1 < low || num1 > high || num2 < low || num2 > high || num3 < 0 || num3 > high {
        return None;
    }
    if !set_range(bits, low, high, num1, num2, num3) {
        buf.unget_char(ch);
        return None;
    }
    Some(ch)
}

/// get_list: range {"," range}, then skip to and over blanks.
fn get_list(buf: &mut Buf, bits: &mut u64, low: i32, high: i32, names: &[&str], mut ch: Ch) -> Option<Ch> {
    *bits = 0;
    loop {
        ch = get_range(buf, bits, low, high, names, ch)?;
        if ch == C(b',') {
            ch = buf.get_char();
        } else {
            break;
        }
    }
    while !matches!(ch, C(b'\t') | C(b' ') | C(b'\n') | C(0) | Eof) {
        ch = buf.get_char();
    }
    while is_blank(ch) {
        ch = buf.get_char();
    }
    Some(ch)
}

/// `parse_cron_entry` (entry.c): None on any syntax error.
pub fn parse_cron_entry(schedule: &str) -> Option<Entry> {
    if schedule.len() >= MAX_FILE_BUFFER_LENGTH {
        return None;
    }
    let mut buf = Buf { data: schedule.as_bytes(), pointer: 0, unget: Vec::new() };
    let mut e = Entry::default();

    buf.skip_comments();
    let mut ch = buf.get_char();
    if ch == Eof {
        return None;
    }

    if ch == C(b'@') {
        let (cmd, _) = buf.get_string(MAX_COMMAND);
        match cmd.as_slice() {
            b"reboot" | b"restart" => e.flags |= WHEN_REBOOT,
            b"yearly" | b"annually" => {
                set_element(&mut e.minute, FIRST_MINUTE, LAST_MINUTE, FIRST_MINUTE);
                set_element(&mut e.hour, FIRST_HOUR, LAST_HOUR, FIRST_HOUR);
                set_element(&mut e.dom, FIRST_DOM, LAST_DOM, FIRST_DOM);
                set_element(&mut e.month, FIRST_MONTH, LAST_MONTH, FIRST_MINUTE);
                set_range(&mut e.dow, FIRST_DOW, LAST_DOW, FIRST_DOW, LAST_DOW, 1);
                e.flags |= DOW_STAR;
            }
            b"monthly" => {
                set_element(&mut e.minute, FIRST_MINUTE, LAST_MINUTE, FIRST_MINUTE);
                set_element(&mut e.hour, FIRST_HOUR, LAST_HOUR, FIRST_HOUR);
                set_element(&mut e.dom, FIRST_DOM, LAST_DOM, FIRST_DOM);
                set_range(&mut e.month, FIRST_MONTH, LAST_MONTH, FIRST_MONTH, LAST_MONTH, 1);
                set_range(&mut e.dow, FIRST_DOW, LAST_DOW, FIRST_DOW, LAST_DOW, 1);
                e.flags |= DOW_STAR;
            }
            b"weekly" => {
                set_element(&mut e.minute, FIRST_MINUTE, LAST_MINUTE, FIRST_MINUTE);
                set_element(&mut e.hour, FIRST_HOUR, LAST_HOUR, FIRST_HOUR);
                set_range(&mut e.dom, FIRST_DOM, LAST_DOM, FIRST_DOM, LAST_DOM, 1);
                set_range(&mut e.month, FIRST_MONTH, LAST_MONTH, FIRST_MONTH, LAST_MONTH, 1);
                set_element(&mut e.dow, FIRST_DOW, LAST_DOW, FIRST_DOW);
                e.flags |= DOW_STAR;
            }
            b"daily" | b"midnight" => {
                set_element(&mut e.minute, FIRST_MINUTE, LAST_MINUTE, FIRST_MINUTE);
                set_element(&mut e.hour, FIRST_HOUR, LAST_HOUR, FIRST_HOUR);
                set_range(&mut e.dom, FIRST_DOM, LAST_DOM, FIRST_DOM, LAST_DOM, 1);
                set_range(&mut e.month, FIRST_MONTH, LAST_MONTH, FIRST_MONTH, LAST_MONTH, 1);
                set_range(&mut e.dow, FIRST_DOW, LAST_DOW, FIRST_DOW, LAST_DOW, 1);
            }
            b"hourly" => {
                set_element(&mut e.minute, FIRST_MINUTE, LAST_MINUTE, FIRST_MINUTE);
                set_range(&mut e.hour, FIRST_HOUR, LAST_HOUR, FIRST_HOUR, LAST_HOUR, 1);
                set_range(&mut e.dom, FIRST_DOM, LAST_DOM, FIRST_DOM, LAST_DOM, 1);
                set_range(&mut e.month, FIRST_MONTH, LAST_MONTH, FIRST_MONTH, LAST_MONTH, 1);
                set_range(&mut e.dow, FIRST_DOW, LAST_DOW, FIRST_DOW, LAST_DOW, 1);
                e.flags |= HR_STAR;
            }
            _ => return None,
        }
    } else {
        if ch == C(b'*') {
            e.flags |= MIN_STAR;
        }
        ch = get_list(&mut buf, &mut e.minute, FIRST_MINUTE, LAST_MINUTE, &[], ch)?;
        if ch == Eof {
            return None;
        }
        if ch == C(b'*') {
            e.flags |= HR_STAR;
        }
        ch = get_list(&mut buf, &mut e.hour, FIRST_HOUR, LAST_HOUR, &[], ch)?;
        if ch == Eof {
            return None;
        }
        if ch == C(b'$') {
            ch = buf.get_char();
            if !is_blank(ch) {
                return None;
            }
            while is_blank(ch) {
                ch = buf.get_char();
            }
            e.flags |= DOM_LAST;
        } else {
            if ch == C(b'*') {
                e.flags |= DOM_STAR;
            }
            ch = get_list(&mut buf, &mut e.dom, FIRST_DOM, LAST_DOM, &[], ch)?;
        }
        if ch == Eof {
            return None;
        }
        ch = get_list(&mut buf, &mut e.month, FIRST_MONTH, LAST_MONTH, &MONTH_NAMES, ch)?;
        if ch == Eof {
            return None;
        }
        if ch == C(b'*') {
            e.flags |= DOW_STAR;
        }
        ch = get_list(&mut buf, &mut e.dow, FIRST_DOW, LAST_DOW, &DOW_NAMES, ch)?;
        if ch == Eof {
            return None;
        }
    }

    if e.dow & 1 != 0 || e.dow & (1 << 7) != 0 {
        e.dow |= 1 | (1 << 7);
    }
    Some(e)
}
